use ruff_diagnostics::{AlwaysAutofixableViolation, Diagnostic, Fix};
use ruff_macros::{derive_message_formats, violation};
use ruff_python_ast::{self as ast, Comprehension, Expr, Ranged};

use crate::checkers::ast::Checker;
use crate::registry::AsRule;
use crate::rules::flake8_comprehensions::fixes;

/// ## What it does
/// Checks for unnecessary `dict`, `list`, and `set` comprehension.
///
/// ## Why is this bad?
/// It's unnecessary to use a `dict`/`list`/`set` comprehension to build a
/// data structure if the elements are unchanged. Wrap the iterable with
/// `dict()`, `list()`, or `set()` instead.
///
/// ## Examples
/// ```python
/// {a: b for a, b in iterable}
/// [x for x in iterable]
/// {x for x in iterable}
/// ```
///
/// Use instead:
/// ```python
/// dict(iterable)
/// list(iterable)
/// set(iterable)
/// ```
#[violation]
pub struct UnnecessaryComprehension {
    obj_type: String,
}

impl AlwaysAutofixableViolation for UnnecessaryComprehension {
    #[derive_message_formats]
    fn message(&self) -> String {
        let UnnecessaryComprehension { obj_type } = self;
        format!("Unnecessary `{obj_type}` comprehension (rewrite using `{obj_type}()`)")
    }

    fn autofix_title(&self) -> String {
        let UnnecessaryComprehension { obj_type } = self;
        format!("Rewrite using `{obj_type}()`")
    }
}

/// Add diagnostic for C416 based on the expression node id.
fn add_diagnostic(checker: &mut Checker, expr: &Expr) {
    let id = match expr {
        Expr::ListComp(_) => "list",
        Expr::SetComp(_) => "set",
        Expr::DictComp(_) => "dict",
        _ => return,
    };
    if !checker.semantic().is_builtin(id) {
        return;
    }
    let mut diagnostic = Diagnostic::new(
        UnnecessaryComprehension {
            obj_type: id.to_string(),
        },
        expr.range(),
    );
    if checker.patch(diagnostic.kind.rule()) {
        diagnostic.try_set_fix(|| {
            fixes::fix_unnecessary_comprehension(checker.locator(), checker.stylist(), expr)
                .map(Fix::suggested)
        });
    }
    checker.diagnostics.push(diagnostic);
}

/// C416
pub(crate) fn unnecessary_dict_comprehension(
    checker: &mut Checker,
    expr: &Expr,
    key: &Expr,
    value: &Expr,
    generators: &[Comprehension],
) {
    let [generator] = generators else {
        return;
    };
    if !generator.ifs.is_empty() || generator.is_async {
        return;
    }
    let Some(key) = key.as_name_expr() else {
        return;
    };
    let Some(value) = value.as_name_expr() else {
        return;
    };
    let Expr::Tuple(ast::ExprTuple { elts, .. }) = &generator.target else {
        return;
    };
    let [target_key, target_value] = elts.as_slice() else {
        return;
    };
    let Some(target_key) = target_key.as_name_expr() else {
        return;
    };
    let Some(target_value) = target_value.as_name_expr() else {
        return;
    };
    if target_key.id != key.id {
        return;
    }
    if target_value.id != value.id {
        return;
    }
    add_diagnostic(checker, expr);
}

/// C416
pub(crate) fn unnecessary_list_set_comprehension(
    checker: &mut Checker,
    expr: &Expr,
    elt: &Expr,
    generators: &[Comprehension],
) {
    let [generator] = generators else {
        return;
    };
    if !generator.ifs.is_empty() || generator.is_async {
        return;
    }
    let Some(elt) = elt.as_name_expr() else {
        return;
    };
    let Some(target) = generator.target.as_name_expr() else {
        return;
    };
    if elt.id != target.id {
        return;
    }
    add_diagnostic(checker, expr);
}
