use anyhow::{bail, format_err, Context, Error};
use clap::{CommandFactory, FromArgMatches};
use ignore::DirEntry;
use imara_diff::intern::InternedInput;
use imara_diff::sink::Counter;
use imara_diff::{diff, Algorithm};
use indicatif::ProgressStyle;
#[cfg_attr(feature = "singlethreaded", allow(unused_imports))]
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use ruff::logging::LogLevel;
use ruff::resolver::python_files_in_path;
use ruff::settings::types::{FilePattern, FilePatternSet};
use ruff_cli::args::{CheckArgs, LogLevelArgs};
use ruff_cli::resolve::resolve;
use ruff_formatter::{FormatError, LineWidth, PrintError};
use ruff_python_formatter::{
    format_module, FormatModuleError, MagicTrailingComma, PyFormatOptions,
};
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::ops::{Add, AddAssign};
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use std::{fmt, fs, io, iter};
use tempfile::NamedTempFile;
use tracing::{debug, error, info, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Find files that ruff would check so we can format them. Adapted from `ruff_cli`.
fn ruff_check_paths(dirs: &[PathBuf]) -> anyhow::Result<Vec<Result<DirEntry, ignore::Error>>> {
    let args_matches = CheckArgs::command()
        .no_binary_name(true)
        .get_matches_from(dirs);
    let check_args: CheckArgs = CheckArgs::from_arg_matches(&args_matches)?;
    let (cli, overrides) = check_args.partition();
    let mut pyproject_config = resolve(
        cli.isolated,
        cli.config.as_deref(),
        &overrides,
        cli.stdin_filename.as_deref(),
    )?;
    // We don't want to format pyproject.toml
    pyproject_config.settings.lib.include = FilePatternSet::try_from_vec(vec![
        FilePattern::Builtin("*.py"),
        FilePattern::Builtin("*.pyi"),
    ])
    .unwrap();
    let (paths, _resolver) = python_files_in_path(&cli.files, &pyproject_config, &overrides)?;
    if paths.is_empty() {
        bail!("no python files in {:?}", dirs)
    }
    Ok(paths)
}

/// Collects statistics over the formatted files to compute the Jaccard index or the similarity
/// index.
///
/// If we define `B` as the black formatted input and `R` as the ruff formatted output, then
/// * `B∩R`: Unchanged lines, neutral in the diff
/// * `B\R`: Black only lines, minus in the diff
/// * `R\B`: Ruff only lines, plus in the diff
///
/// The [Jaccard index](https://en.wikipedia.org/wiki/Jaccard_index) can be defined as
/// ```text
/// J(B, R) = |B∩R| / (|B\R| + |R\B| + |B∩R|)
/// ```
/// which you can read as number unchanged lines in the diff divided by all lines in the diff. If
/// the input is not black formatted, this only becomes a measure for the changes made to the
/// codebase during the initial formatting.
///
/// Another measure is the similarity index, the percentage of unchanged lines. We compute it as
/// ```text
/// Sim(B, R) = |B∩R| / (|B\R| + |B∩R|)
/// ```
/// which you can alternatively read as all lines in the input
#[derive(Default, Debug, Copy, Clone)]
pub(crate) struct Statistics {
    /// The size of `A\B`, the number of lines only in the input, which we assume to be black
    /// formatted
    black_input: u32,
    /// The size of `B\A`, the number of lines only in the formatted output
    ruff_output: u32,
    /// The number of matching identical lines
    intersection: u32,
}

impl Statistics {
    pub(crate) fn from_versions(black: &str, ruff: &str) -> Self {
        if black == ruff {
            let intersection = u32::try_from(black.lines().count()).unwrap();
            Self {
                black_input: 0,
                ruff_output: 0,
                intersection,
            }
        } else {
            // `similar` was too slow (for some files >90% diffing instead of formatting)
            let input = InternedInput::new(black, ruff);
            let changes = diff(Algorithm::Histogram, &input, Counter::default());
            assert_eq!(
                input.before.len() - (changes.removals as usize),
                input.after.len() - (changes.insertions as usize)
            );
            Self {
                black_input: changes.removals,
                ruff_output: changes.insertions,
                intersection: u32::try_from(input.before.len()).unwrap() - changes.removals,
            }
        }
    }

    /// We currently prefer the the similarity index, but i'd like to keep this around
    #[allow(clippy::cast_precision_loss, unused)]
    pub(crate) fn jaccard_index(&self) -> f32 {
        self.intersection as f32 / (self.black_input + self.ruff_output + self.intersection) as f32
    }

    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn similarity_index(&self) -> f32 {
        self.intersection as f32 / (self.black_input + self.intersection) as f32
    }
}

impl Add<Statistics> for Statistics {
    type Output = Statistics;

    fn add(self, rhs: Statistics) -> Self::Output {
        Statistics {
            black_input: self.black_input + rhs.black_input,
            ruff_output: self.ruff_output + rhs.ruff_output,
            intersection: self.intersection + rhs.intersection,
        }
    }
}

impl AddAssign<Statistics> for Statistics {
    fn add_assign(&mut self, rhs: Statistics) {
        *self = *self + rhs;
    }
}

/// Control the verbosity of the output
#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum, Default)]
pub(crate) enum Format {
    /// Filenames only
    Minimal,
    /// Filenames and reduced diff
    #[default]
    Default,
    /// Full diff and invalid code
    Full,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(clap::Args)]
pub(crate) struct Args {
    /// Like `ruff check`'s files. See `--multi-project` if you want to format an ecosystem
    /// checkout.
    pub(crate) files: Vec<PathBuf>,
    /// Check stability
    ///
    /// We want to ensure that once formatted content stays the same when formatted again, which is
    /// known as formatter stability or formatter idempotency, and that the formatter prints
    /// syntactically valid code. As our test cases cover only a limited amount of code, this allows
    /// checking entire repositories.
    #[arg(long)]
    pub(crate) stability_check: bool,
    /// Format the files. Without this flag, the python files are not modified
    #[arg(long)]
    pub(crate) write: bool,
    /// Control the verbosity of the output
    #[arg(long, default_value_t, value_enum)]
    pub(crate) format: Format,
    /// Print only the first error and exit, `-x` is same as pytest
    #[arg(long, short = 'x')]
    pub(crate) exit_first_error: bool,
    /// Checks each project inside a directory, useful e.g. if you want to check all of the
    /// ecosystem checkouts.
    #[arg(long)]
    pub(crate) multi_project: bool,
    /// Write all errors to this file in addition to stdout. Only used in multi-project mode.
    #[arg(long)]
    pub(crate) error_file: Option<PathBuf>,
    /// Write all log messages (same as cli) to this file
    #[arg(long)]
    pub(crate) log_file: Option<PathBuf>,
    /// Write a markdown table with the similarity indices to this file
    #[arg(long)]
    pub(crate) stats_file: Option<PathBuf>,
    /// Assert that there are exactly this many input files with errors. This catches regressions
    /// (or improvements) in the parser.
    #[arg(long)]
    pub(crate) files_with_errors: Option<u32>,
    #[clap(flatten)]
    pub(crate) log_level_args: LogLevelArgs,
}

pub(crate) fn main(args: &Args) -> anyhow::Result<ExitCode> {
    setup_logging(&args.log_level_args, args.log_file.as_deref())?;

    let mut error_file = match &args.error_file {
        Some(error_file) => Some(BufWriter::new(
            File::create(error_file).context("Couldn't open error file")?,
        )),
        None => None,
    };

    let all_success = if args.multi_project {
        format_dev_multi_project(args, error_file)?
    } else {
        let result = format_dev_project(&args.files, args.stability_check, args.write)?;
        let error_count = result.error_count();

        if result.error_count() > 0 {
            error!(parent: None, "{}", result.display(args.format));
        }
        if let Some(error_file) = &mut error_file {
            write!(error_file, "{}", result.display(args.format)).unwrap();
        }
        info!(
            parent: None,
            "Done: {} stability errors, {} files, similarity index {:.5}), took {:.2}s, {} input files contained syntax errors ",
            error_count,
            result.file_count,
            result.statistics.similarity_index(),
            result.duration.as_secs_f32(),
            result.syntax_error_in_input,
        );

        if let Some(files_with_errors) = args.files_with_errors {
            if result.syntax_error_in_input != files_with_errors {
                error!(
                    "Expected {files_with_errors} input files with errors, found {}",
                    result.syntax_error_in_input
                );
                return Ok(ExitCode::FAILURE);
            }
        }

        error_count == 0
    };
    if all_success {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

fn setup_logging(log_level_args: &LogLevelArgs, log_file: Option<&Path>) -> io::Result<()> {
    // Custom translation since we need the tracing type for `EnvFilter`
    let log_level = match LogLevel::from(log_level_args) {
        LogLevel::Default => tracing::Level::INFO,
        LogLevel::Verbose => tracing::Level::DEBUG,
        LogLevel::Quiet => tracing::Level::WARN,
        LogLevel::Silent => tracing::Level::ERROR,
    };
    // 1. `RUST_LOG=`, 2. explicit CLI log level, 3. info, the ruff_cli default
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::builder()
            .with_default_directive(log_level.into())
            .parse_lossy("")
    });
    let indicatif_layer = IndicatifLayer::new().with_progress_style(
        // Default without the spinner
        ProgressStyle::with_template("{span_child_prefix} {span_name}{{{span_fields}}}").unwrap(),
    );
    let indicitif_compatible_writer_layer = tracing_subscriber::fmt::layer()
        .with_writer(indicatif_layer.get_stderr_writer())
        .with_target(false);
    let log_layer = log_file.map(File::create).transpose()?.map(|log_file| {
        tracing_subscriber::fmt::layer()
            .with_writer(log_file)
            .with_ansi(false)
    });
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(indicitif_compatible_writer_layer)
        .with(indicatif_layer)
        .with(log_layer)
        .init();
    Ok(())
}

/// Checks a directory of projects
fn format_dev_multi_project(
    args: &Args,
    mut error_file: Option<BufWriter<File>>,
) -> anyhow::Result<bool> {
    let mut total_errors = 0;
    let mut total_files = 0;
    let mut total_syntax_error_in_input = 0;
    let start = Instant::now();

    let mut project_paths = Vec::new();

    for directory in &args.files {
        for project_directory in directory
            .read_dir()
            .with_context(|| "Failed to read projects directory '{directory}'")?
        {
            project_paths.push(
                project_directory
                    .with_context(|| "Failed to read project directory '{project_directory}'")?
                    .path(),
            );
        }
    }

    let pb_span = info_span!("format_dev_multi_project progress bar");
    pb_span.pb_set_style(&ProgressStyle::default_bar());
    pb_span.pb_set_length(project_paths.len() as u64);
    let pb_span_enter = pb_span.enter();

    let mut results = Vec::new();

    for project_path in project_paths {
        debug!(parent: None, "Starting {}", project_path.display());

        match format_dev_project(&[project_path.clone()], args.stability_check, args.write) {
            Ok(result) => {
                total_errors += result.error_count();
                total_files += result.file_count;
                total_syntax_error_in_input += result.syntax_error_in_input;

                info!(
                    parent: None,
                    "Finished {}: {} stability errors, {} files, similarity index {:.5}), took {:.2}s, {} input files contained syntax errors ",
                    project_path.display(),
                    result.error_count(),
                    result.file_count,
                    result.statistics.similarity_index(),
                    result.duration.as_secs_f32(),
                    result.syntax_error_in_input,
                );
                if result.error_count() > 0 {
                    error!(
                        parent: None,
                        "{}",
                        result.display(args.format).to_string().trim_end()
                    );
                }
                if let Some(error_file) = &mut error_file {
                    write!(error_file, "{}", result.display(args.format)).unwrap();
                }
                results.push(result);

                pb_span.pb_inc(1);
            }
            Err(error) => {
                error!(parent: None, "Failed {}: {}", project_path.display(), error);
                pb_span.pb_inc(1);
            }
        }
    }

    drop(pb_span_enter);
    drop(pb_span);

    let duration = start.elapsed();

    info!(
        parent: None,
        "Finished: {total_errors} stability errors, {total_files} files, tool {}s, {total_syntax_error_in_input} input files contained syntax errors ",
        duration.as_secs_f32(),
    );

    if let Some(stats_file) = &args.stats_file {
        results.sort_by(|result1, result2| result1.name.cmp(&result2.name));
        let project_col_len = results
            .iter()
            .map(|result| result.name.len())
            .chain(iter::once("project".len()))
            .max()
            .unwrap_or_default();
        let mut stats_file = BufWriter::new(File::create(stats_file)?);
        writeln!(
            stats_file,
            "| {:<project_col_len$} | similarity index |",
            "project"
        )?;
        writeln!(
            stats_file,
            "|-{:-<project_col_len$}-|------------------|",
            ""
        )?;
        for result in results {
            writeln!(
                stats_file,
                "| {:<project_col_len$} | {:.5}          |",
                result.name,
                result.statistics.similarity_index()
            )?;
        }
    }

    if let Some(files_with_errors) = args.files_with_errors {
        if total_syntax_error_in_input != files_with_errors {
            error!(
                "Expected {files_with_errors} input files with errors, found {}",
                total_syntax_error_in_input
            );
            return Ok(false);
        }
    }

    Ok(total_errors == 0)
}

#[tracing::instrument]
fn format_dev_project(
    files: &[PathBuf],
    stability_check: bool,
    write: bool,
) -> anyhow::Result<CheckRepoResult> {
    let start = Instant::now();

    // TODO(konstin): The assumptions between this script (one repo) and ruff (pass in a bunch of
    // files) mismatch.
    let black_options = BlackOptions::from_file(&files[0])?;
    debug!(
        parent: None,
        "Options for {}: {black_options:?}",
        files[0].display()
    );

    // TODO(konstin): black excludes

    // Find files to check (or in this case, format twice). Adapted from ruff_cli
    // First argument is ignored
    let paths = ruff_check_paths(files)?;

    let results = {
        let pb_span =
            info_span!("format_dev_project progress bar", first_file = %files[0].display());
        pb_span.pb_set_style(&ProgressStyle::default_bar());
        pb_span.pb_set_length(paths.len() as u64);
        let _pb_span_enter = pb_span.enter();
        #[cfg(not(feature = "singlethreaded"))]
        let iter = { paths.into_par_iter() };
        #[cfg(feature = "singlethreaded")]
        let iter = { paths.into_iter() };
        iter.map(|dir_entry| {
            let result = format_dir_entry(dir_entry, stability_check, write, &black_options);
            pb_span.pb_inc(1);
            result
        })
        .collect::<anyhow::Result<Vec<_>>>()?
    };

    let mut statistics = Statistics::default();
    let mut formatted_counter = 0;
    let mut syntax_error_in_input = 0;
    let mut diagnostics = Vec::new();
    for (result, file) in results {
        formatted_counter += 1;
        match result {
            Ok(statistics_file) => statistics += statistics_file,
            Err(error) => {
                match error {
                    CheckFileError::SyntaxErrorInInput(error) => {
                        // This is not our error
                        debug!(
                            parent: None,
                            "Syntax error in {}: {}",
                            file.display(),
                            error
                        );
                        syntax_error_in_input += 1;
                    }
                    _ => diagnostics.push(Diagnostic { file, error }),
                }
            }
        }
    }

    let duration = start.elapsed();

    let name = files[0]
        .file_name()
        .unwrap_or(files[0].as_os_str())
        .to_string_lossy()
        .to_string();
    Ok(CheckRepoResult {
        name,
        duration,
        file_count: formatted_counter,
        diagnostics,
        statistics,
        syntax_error_in_input,
    })
}

/// Error handling in between walkdir and `format_dev_file`
fn format_dir_entry(
    dir_entry: Result<DirEntry, ignore::Error>,
    stability_check: bool,
    write: bool,
    options: &BlackOptions,
) -> anyhow::Result<(Result<Statistics, CheckFileError>, PathBuf), Error> {
    let dir_entry = match dir_entry.context("Iterating the files in the repository failed") {
        Ok(dir_entry) => dir_entry,
        Err(err) => return Err(err),
    };
    let file = dir_entry.path().to_path_buf();
    // For some reason it does not filter in the beginning
    if dir_entry.file_name() == "pyproject.toml" {
        return Ok((Ok(Statistics::default()), file));
    }

    let file = dir_entry.path().to_path_buf();
    let options = options.to_py_format_options(&file);
    // Handle panics (mostly in `debug_assert!`)
    let result = match catch_unwind(|| format_dev_file(&file, stability_check, write, options)) {
        Ok(result) => result,
        Err(panic) => {
            if let Some(message) = panic.downcast_ref::<String>() {
                Err(CheckFileError::Panic {
                    message: message.clone(),
                })
            } else if let Some(&message) = panic.downcast_ref::<&str>() {
                Err(CheckFileError::Panic {
                    message: message.to_string(),
                })
            } else {
                Err(CheckFileError::Panic {
                    // This should not happen, but it can
                    message: "(Panic didn't set a string message)".to_string(),
                })
            }
        }
    };
    Ok((result, file))
}

/// A compact diff that only shows a header and changes, but nothing unchanged. This makes viewing
/// multiple errors easier.
fn diff_show_only_changes(
    writer: &mut Formatter,
    formatted: &str,
    reformatted: &str,
) -> fmt::Result {
    for changes in TextDiff::from_lines(formatted, reformatted)
        .unified_diff()
        .iter_hunks()
    {
        for (idx, change) in changes
            .iter_changes()
            .filter(|change| change.tag() != ChangeTag::Equal)
            .enumerate()
        {
            if idx == 0 {
                writeln!(writer, "{}", changes.header())?;
            }
            write!(writer, "{}", change.tag())?;
            writer.write_str(change.value())?;
        }
    }
    Ok(())
}

struct CheckRepoResult {
    name: String,
    duration: Duration,
    file_count: usize,
    diagnostics: Vec<Diagnostic>,
    statistics: Statistics,
    syntax_error_in_input: u32,
}

impl CheckRepoResult {
    fn display(&self, format: Format) -> DisplayCheckRepoResult {
        DisplayCheckRepoResult {
            result: self,
            format,
        }
    }

    /// Count the actual errors excluding invalid input files and io errors
    fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostics| !diagnostics.error.is_success())
            .count()
    }
}

struct DisplayCheckRepoResult<'a> {
    result: &'a CheckRepoResult,
    format: Format,
}

impl Display for DisplayCheckRepoResult<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for diagnostic in &self.result.diagnostics {
            write!(f, "{}", diagnostic.display(self.format))?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Diagnostic {
    file: PathBuf,
    error: CheckFileError,
}

impl Diagnostic {
    fn display(&self, format: Format) -> DisplayDiagnostic {
        DisplayDiagnostic {
            diagnostic: self,
            format,
        }
    }
}

struct DisplayDiagnostic<'a> {
    format: Format,
    diagnostic: &'a Diagnostic,
}

impl Display for DisplayDiagnostic<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let Diagnostic { file, error } = &self.diagnostic;

        match error {
            CheckFileError::Unstable {
                formatted,
                reformatted,
            } => {
                writeln!(f, "Unstable formatting {}", file.display())?;
                match self.format {
                    Format::Minimal => {}
                    Format::Default => {
                        diff_show_only_changes(f, formatted, reformatted)?;
                    }
                    Format::Full => {
                        let diff = TextDiff::from_lines(formatted.as_str(), reformatted.as_str())
                            .unified_diff()
                            .header("Formatted once", "Formatted twice")
                            .to_string();
                        writeln!(
                            f,
                            r#"Reformatting the formatted code a second time resulted in formatting changes.
---
{diff}---

Formatted once:
---
{formatted}---

Formatted twice:
---
{reformatted}---\n"#,
                        )?;
                    }
                }
            }
            CheckFileError::Panic { message } => {
                writeln!(f, "Panic {}: {}", file.display(), message)?;
            }
            CheckFileError::SyntaxErrorInInput(error) => {
                writeln!(f, "Syntax error in {}: {}", file.display(), error)?;
            }
            CheckFileError::SyntaxErrorInOutput { formatted, error } => {
                writeln!(
                    f,
                    "Formatter generated invalid syntax {}: {}",
                    file.display(),
                    error
                )?;
                if self.format == Format::Full {
                    writeln!(f, "---\n{formatted}\n---\n")?;
                }
            }
            CheckFileError::FormatError(error) => {
                writeln!(f, "Formatter error for {}: {}", file.display(), error)?;
            }
            CheckFileError::PrintError(error) => {
                writeln!(f, "Printer error for {}: {}", file.display(), error)?;
            }
            CheckFileError::IoError(error) => {
                writeln!(f, "Error reading {}: {}", file.display(), error)?;
            }
            #[cfg(not(debug_assertions))]
            CheckFileError::Slow(duration) => {
                writeln!(
                    f,
                    "Slow formatting {}: Formatting the file took {}ms",
                    file.display(),
                    duration.as_millis()
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
enum CheckFileError {
    /// First and second pass of the formatter are different
    Unstable {
        formatted: String,
        reformatted: String,
    },
    /// The input file was already invalid (not a bug)
    SyntaxErrorInInput(FormatModuleError),
    /// The formatter introduced a syntax error
    SyntaxErrorInOutput {
        formatted: String,
        error: FormatModuleError,
    },
    /// The formatter failed (bug)
    FormatError(FormatError),
    /// The printer failed (bug)
    PrintError(PrintError),
    /// Failed to read the file, this sometimes happens e.g. with strange filenames (not a bug)
    IoError(io::Error),
    /// From `catch_unwind`
    Panic { message: String },

    /// Formatting a file took too long
    #[cfg(not(debug_assertions))]
    Slow(Duration),
}

impl CheckFileError {
    /// Returns `false` if this is a formatter bug or `true` is if it is something outside of ruff
    fn is_success(&self) -> bool {
        match self {
            CheckFileError::SyntaxErrorInInput(_) | CheckFileError::IoError(_) => true,
            CheckFileError::Unstable { .. }
            | CheckFileError::SyntaxErrorInOutput { .. }
            | CheckFileError::FormatError(_)
            | CheckFileError::PrintError(_)
            | CheckFileError::Panic { .. } => false,
            #[cfg(not(debug_assertions))]
            CheckFileError::Slow(_) => false,
        }
    }
}

impl From<io::Error> for CheckFileError {
    fn from(value: io::Error) -> Self {
        Self::IoError(value)
    }
}

#[tracing::instrument(skip_all, fields(input_path = % input_path.display()))]
fn format_dev_file(
    input_path: &Path,
    stability_check: bool,
    write: bool,
    options: PyFormatOptions,
) -> Result<Statistics, CheckFileError> {
    let content = fs::read_to_string(input_path)?;
    #[cfg(not(debug_assertions))]
    let start = Instant::now();
    let printed = match format_module(&content, options.clone()) {
        Ok(printed) => printed,
        Err(err @ (FormatModuleError::LexError(_) | FormatModuleError::ParseError(_))) => {
            return Err(CheckFileError::SyntaxErrorInInput(err));
        }
        Err(FormatModuleError::FormatError(err)) => {
            return Err(CheckFileError::FormatError(err));
        }
        Err(FormatModuleError::PrintError(err)) => {
            return Err(CheckFileError::PrintError(err));
        }
    };
    let formatted = printed.as_code();
    #[cfg(not(debug_assertions))]
    let format_duration = Instant::now() - start;

    if write && content != formatted {
        // Simple atomic write.
        // The file is in a directory so it must have a parent. Surprisingly `DirEntry` doesn't
        // give us access without unwrap
        let mut file = NamedTempFile::new_in(input_path.parent().unwrap())?;
        file.write_all(formatted.as_bytes())?;
        // "If a file exists at the target path, persist will atomically replace it."
        file.persist(input_path).map_err(|error| error.error)?;
    }

    if stability_check {
        let reformatted = match format_module(formatted, options) {
            Ok(reformatted) => reformatted,
            Err(err @ (FormatModuleError::LexError(_) | FormatModuleError::ParseError(_))) => {
                return Err(CheckFileError::SyntaxErrorInOutput {
                    formatted: formatted.to_string(),
                    error: err,
                });
            }
            Err(FormatModuleError::FormatError(err)) => {
                return Err(CheckFileError::FormatError(err));
            }
            Err(FormatModuleError::PrintError(err)) => {
                return Err(CheckFileError::PrintError(err));
            }
        };

        if reformatted.as_code() != formatted {
            return Err(CheckFileError::Unstable {
                formatted: formatted.to_string(),
                reformatted: reformatted.into_code(),
            });
        }
    }

    #[cfg(not(debug_assertions))]
    if format_duration > Duration::from_millis(50) {
        return Err(CheckFileError::Slow(format_duration));
    }

    Ok(Statistics::from_versions(&content, formatted))
}

#[derive(Deserialize, Default)]
struct PyprojectToml {
    tool: Option<PyprojectTomlTool>,
}

#[derive(Deserialize, Default)]
struct PyprojectTomlTool {
    black: Option<BlackOptions>,
}

#[derive(Deserialize, Debug)]
#[serde(default)]
struct BlackOptions {
    // Black actually allows both snake case and kebab case
    #[serde(alias = "line-length")]
    line_length: u16,
    #[serde(alias = "skip-magic-trailing-comma")]
    skip_magic_trailing_comma: bool,
    #[allow(unused)]
    #[serde(alias = "force-exclude")]
    force_exclude: Option<String>,
}

impl Default for BlackOptions {
    fn default() -> Self {
        Self {
            line_length: 88,
            skip_magic_trailing_comma: false,
            force_exclude: None,
        }
    }
}

impl BlackOptions {
    /// TODO(konstin): For the real version, fix printing of error chains and remove the path
    /// argument
    fn from_toml(toml: &str, path: &Path) -> anyhow::Result<Self> {
        let pyproject_toml: PyprojectToml = toml::from_str(toml).map_err(|e| {
            format_err!(
                "Not a valid pyproject.toml toml file at {}: {e}",
                path.display()
            )
        })?;
        let black_options = pyproject_toml
            .tool
            .unwrap_or_default()
            .black
            .unwrap_or_default();
        debug!(
            "Found {}, setting black options: {:?}",
            path.display(),
            &black_options
        );
        Ok(black_options)
    }

    fn from_file(repo: &Path) -> anyhow::Result<Self> {
        let path = repo.join("pyproject.toml");
        if !path.is_file() {
            debug!(
                "No pyproject.toml at {}, using black option defaults",
                path.display()
            );
            return Ok(Self::default());
        }
        Self::from_toml(&fs::read_to_string(&path)?, repo)
    }

    fn to_py_format_options(&self, file: &Path) -> PyFormatOptions {
        PyFormatOptions::from_extension(file)
            .with_line_width(
                LineWidth::try_from(self.line_length).expect("Invalid line length limit"),
            )
            .with_magic_trailing_comma(if self.skip_magic_trailing_comma {
                MagicTrailingComma::Ignore
            } else {
                MagicTrailingComma::Respect
            })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use indoc::indoc;

    use ruff_formatter::{FormatOptions, LineWidth};
    use ruff_python_formatter::MagicTrailingComma;

    use crate::format_dev::BlackOptions;

    #[test]
    fn test_transformers() {
        let toml = indoc! {"
            [tool.black]
            line-length = 119
            target-version = ['py37']
        "};
        let options = BlackOptions::from_toml(toml, Path::new("pyproject.toml"))
            .unwrap()
            .to_py_format_options(Path::new("code_inline.py"));
        assert_eq!(options.line_width(), LineWidth::try_from(119).unwrap());
        assert!(matches!(
            options.magic_trailing_comma(),
            MagicTrailingComma::Respect
        ));
    }

    #[test]
    fn test_typeshed() {
        let toml = indoc! {r#"
            [tool.black]
            line_length = 130
            target_version = ["py310"]
            skip_magic_trailing_comma = true
            force-exclude = ".*_pb2.pyi"
        "#};
        let options = BlackOptions::from_toml(toml, Path::new("pyproject.toml"))
            .unwrap()
            .to_py_format_options(Path::new("code_inline.py"));
        assert_eq!(options.line_width(), LineWidth::try_from(130).unwrap());
        assert!(matches!(
            options.magic_trailing_comma(),
            MagicTrailingComma::Ignore
        ));
    }
}
