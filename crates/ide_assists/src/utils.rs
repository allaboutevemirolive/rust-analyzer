//! Assorted functions shared by several assists.

pub(crate) mod suggest_name;
mod gen_trait_fn_body;

use std::ops;

use hir::HasSource;
use ide_db::{helpers::SnippetCap, path_transform::PathTransform, RootDatabase};
use itertools::Itertools;
use stdx::format_to;
use syntax::{
    ast::{
        self,
        edit::{self, AstNodeEdit},
        edit_in_place::AttrsOwnerEdit,
        make, HasArgList, HasAttrs, HasGenericParams, HasName, HasTypeBounds, Whitespace,
    },
    ted, AstNode, AstToken, Direction, SmolStr, SourceFile,
    SyntaxKind::*,
    SyntaxNode, TextRange, TextSize, T,
};

use crate::assist_context::{AssistBuilder, AssistContext};

pub(crate) use gen_trait_fn_body::gen_trait_fn_body;

pub(crate) fn unwrap_trivial_block(block_expr: ast::BlockExpr) -> ast::Expr {
    extract_trivial_expression(&block_expr)
        .filter(|expr| !expr.syntax().text().contains_char('\n'))
        .unwrap_or_else(|| block_expr.into())
}

pub fn extract_trivial_expression(block_expr: &ast::BlockExpr) -> Option<ast::Expr> {
    if block_expr.modifier().is_some() {
        return None;
    }
    let stmt_list = block_expr.stmt_list()?;
    let has_anything_else = |thing: &SyntaxNode| -> bool {
        let mut non_trivial_children =
            stmt_list.syntax().children_with_tokens().filter(|it| match it.kind() {
                WHITESPACE | T!['{'] | T!['}'] => false,
                _ => it.as_node() != Some(thing),
            });
        non_trivial_children.next().is_some()
    };

    if let Some(expr) = stmt_list.tail_expr() {
        if has_anything_else(expr.syntax()) {
            return None;
        }
        return Some(expr);
    }
    // Unwrap `{ continue; }`
    let stmt = stmt_list.statements().next()?;
    if let ast::Stmt::ExprStmt(expr_stmt) = stmt {
        if has_anything_else(expr_stmt.syntax()) {
            return None;
        }
        let expr = expr_stmt.expr()?;
        if matches!(expr.syntax().kind(), CONTINUE_EXPR | BREAK_EXPR | RETURN_EXPR) {
            return Some(expr);
        }
    }
    None
}

/// This is a method with a heuristics to support test methods annotated with custom test annotations, such as
/// `#[test_case(...)]`, `#[tokio::test]` and similar.
/// Also a regular `#[test]` annotation is supported.
///
/// It may produce false positives, for example, `#[wasm_bindgen_test]` requires a different command to run the test,
/// but it's better than not to have the runnables for the tests at all.
pub fn test_related_attribute(fn_def: &ast::Fn) -> Option<ast::Attr> {
    fn_def.attrs().find_map(|attr| {
        let path = attr.path()?;
        let text = path.syntax().text().to_string();
        if text.starts_with("test") || text.ends_with("test") {
            Some(attr)
        } else {
            None
        }
    })
}

#[derive(Copy, Clone, PartialEq)]
pub enum DefaultMethods {
    Only,
    No,
}

pub fn filter_assoc_items(
    db: &RootDatabase,
    items: &[hir::AssocItem],
    default_methods: DefaultMethods,
) -> Vec<ast::AssocItem> {
    fn has_def_name(item: &ast::AssocItem) -> bool {
        match item {
            ast::AssocItem::Fn(def) => def.name(),
            ast::AssocItem::TypeAlias(def) => def.name(),
            ast::AssocItem::Const(def) => def.name(),
            ast::AssocItem::MacroCall(_) => None,
        }
        .is_some()
    }

    items
        .iter()
        // Note: This throws away items with no source.
        .filter_map(|i| {
            let item = match i {
                hir::AssocItem::Function(i) => ast::AssocItem::Fn(i.source(db)?.value),
                hir::AssocItem::TypeAlias(i) => ast::AssocItem::TypeAlias(i.source(db)?.value),
                hir::AssocItem::Const(i) => ast::AssocItem::Const(i.source(db)?.value),
            };
            Some(item)
        })
        .filter(has_def_name)
        .filter(|it| match it {
            ast::AssocItem::Fn(def) => matches!(
                (default_methods, def.body()),
                (DefaultMethods::Only, Some(_)) | (DefaultMethods::No, None)
            ),
            _ => default_methods == DefaultMethods::No,
        })
        .collect::<Vec<_>>()
}

pub fn add_trait_assoc_items_to_impl(
    sema: &hir::Semantics<ide_db::RootDatabase>,
    items: Vec<ast::AssocItem>,
    trait_: hir::Trait,
    impl_: ast::Impl,
    target_scope: hir::SemanticsScope,
) -> (ast::Impl, ast::AssocItem) {
    let source_scope = sema.scope_for_def(trait_);

    let transform = PathTransform::trait_impl(&target_scope, &source_scope, trait_, impl_.clone());

    let items = items.into_iter().map(|assoc_item| {
        let assoc_item = assoc_item.clone_for_update();
        transform.apply(assoc_item.syntax());
        assoc_item.remove_attrs_and_docs();
        assoc_item
    });

    let res = impl_.clone_for_update();

    let assoc_item_list = res.get_or_create_assoc_item_list();
    let mut first_item = None;
    for item in items {
        first_item.get_or_insert_with(|| item.clone());
        match &item {
            ast::AssocItem::Fn(fn_) if fn_.body().is_none() => {
                let body = make::block_expr(None, Some(make::ext::expr_todo()))
                    .indent(edit::IndentLevel(1));
                ted::replace(fn_.get_or_create_body().syntax(), body.clone_for_update().syntax())
            }
            ast::AssocItem::TypeAlias(type_alias) => {
                if let Some(type_bound_list) = type_alias.type_bound_list() {
                    type_bound_list.remove()
                }
            }
            _ => {}
        }

        assoc_item_list.add_item(item)
    }

    (res, first_item.unwrap())
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Cursor<'a> {
    Replace(&'a SyntaxNode),
    Before(&'a SyntaxNode),
}

impl<'a> Cursor<'a> {
    fn node(self) -> &'a SyntaxNode {
        match self {
            Cursor::Replace(node) | Cursor::Before(node) => node,
        }
    }
}

pub(crate) fn render_snippet(_cap: SnippetCap, node: &SyntaxNode, cursor: Cursor) -> String {
    assert!(cursor.node().ancestors().any(|it| it == *node));
    let range = cursor.node().text_range() - node.text_range().start();
    let range: ops::Range<usize> = range.into();

    let mut placeholder = cursor.node().to_string();
    escape(&mut placeholder);
    let tab_stop = match cursor {
        Cursor::Replace(placeholder) => format!("${{0:{}}}", placeholder),
        Cursor::Before(placeholder) => format!("$0{}", placeholder),
    };

    let mut buf = node.to_string();
    buf.replace_range(range, &tab_stop);
    return buf;

    fn escape(buf: &mut String) {
        stdx::replace(buf, '{', r"\{");
        stdx::replace(buf, '}', r"\}");
        stdx::replace(buf, '$', r"\$");
    }
}

pub(crate) fn vis_offset(node: &SyntaxNode) -> TextSize {
    node.children_with_tokens()
        .find(|it| !matches!(it.kind(), WHITESPACE | COMMENT | ATTR))
        .map(|it| it.text_range().start())
        .unwrap_or_else(|| node.text_range().start())
}

pub(crate) fn invert_boolean_expression(expr: ast::Expr) -> ast::Expr {
    invert_special_case(&expr).unwrap_or_else(|| make::expr_prefix(T![!], expr))
}

fn invert_special_case(expr: &ast::Expr) -> Option<ast::Expr> {
    match expr {
        ast::Expr::BinExpr(bin) => {
            let bin = bin.clone_for_update();
            let op_token = bin.op_token()?;
            let rev_token = match op_token.kind() {
                T![==] => T![!=],
                T![!=] => T![==],
                T![<] => T![>=],
                T![<=] => T![>],
                T![>] => T![<=],
                T![>=] => T![<],
                // Parenthesize other expressions before prefixing `!`
                _ => return Some(make::expr_prefix(T![!], make::expr_paren(expr.clone()))),
            };
            ted::replace(op_token, make::token(rev_token));
            Some(bin.into())
        }
        ast::Expr::MethodCallExpr(mce) => {
            let receiver = mce.receiver()?;
            let method = mce.name_ref()?;
            let arg_list = mce.arg_list()?;

            let method = match method.text().as_str() {
                "is_some" => "is_none",
                "is_none" => "is_some",
                "is_ok" => "is_err",
                "is_err" => "is_ok",
                _ => return None,
            };
            Some(make::expr_method_call(receiver, make::name_ref(method), arg_list))
        }
        ast::Expr::PrefixExpr(pe) if pe.op_kind()? == ast::UnaryOp::Not => match pe.expr()? {
            ast::Expr::ParenExpr(parexpr) => parexpr.expr(),
            _ => pe.expr(),
        },
        ast::Expr::Literal(lit) => match lit.kind() {
            ast::LiteralKind::Bool(b) => match b {
                true => Some(ast::Expr::Literal(make::expr_literal("false"))),
                false => Some(ast::Expr::Literal(make::expr_literal("true"))),
            },
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn next_prev() -> impl Iterator<Item = Direction> {
    [Direction::Next, Direction::Prev].iter().copied()
}

pub(crate) fn does_pat_match_variant(pat: &ast::Pat, var: &ast::Pat) -> bool {
    let first_node_text = |pat: &ast::Pat| pat.syntax().first_child().map(|node| node.text());

    let pat_head = match pat {
        ast::Pat::IdentPat(bind_pat) => match bind_pat.pat() {
            Some(p) => first_node_text(&p),
            None => return pat.syntax().text() == var.syntax().text(),
        },
        pat => first_node_text(pat),
    };

    let var_head = first_node_text(var);

    pat_head == var_head
}

pub(crate) fn does_nested_pattern(pat: &ast::Pat) -> bool {
    let depth = calc_depth(pat, 0);

    if 1 < depth {
        return true;
    }
    false
}

fn calc_depth(pat: &ast::Pat, depth: usize) -> usize {
    match pat {
        ast::Pat::IdentPat(_)
        | ast::Pat::BoxPat(_)
        | ast::Pat::RestPat(_)
        | ast::Pat::LiteralPat(_)
        | ast::Pat::MacroPat(_)
        | ast::Pat::OrPat(_)
        | ast::Pat::ParenPat(_)
        | ast::Pat::PathPat(_)
        | ast::Pat::WildcardPat(_)
        | ast::Pat::RangePat(_)
        | ast::Pat::RecordPat(_)
        | ast::Pat::RefPat(_)
        | ast::Pat::SlicePat(_)
        | ast::Pat::TuplePat(_)
        | ast::Pat::ConstBlockPat(_) => depth,

        // FIXME: Other patterns may also be nested. Currently it simply supports only `TupleStructPat`
        ast::Pat::TupleStructPat(pat) => {
            let mut max_depth = depth;
            for p in pat.fields() {
                let d = calc_depth(&p, depth + 1);
                if d > max_depth {
                    max_depth = d
                }
            }
            max_depth
        }
    }
}

// Uses a syntax-driven approach to find any impl blocks for the struct that
// exist within the module/file
//
// Returns `None` if we've found an existing fn
//
// FIXME: change the new fn checking to a more semantic approach when that's more
// viable (e.g. we process proc macros, etc)
// FIXME: this partially overlaps with `find_impl_block_*`
pub(crate) fn find_struct_impl(
    ctx: &AssistContext,
    adt: &ast::Adt,
    name: &str,
) -> Option<Option<ast::Impl>> {
    let db = ctx.db();
    let module = adt.syntax().parent()?;

    let struct_def = ctx.sema.to_def(adt)?;

    let block = module.descendants().filter_map(ast::Impl::cast).find_map(|impl_blk| {
        let blk = ctx.sema.to_def(&impl_blk)?;

        // FIXME: handle e.g. `struct S<T>; impl<U> S<U> {}`
        // (we currently use the wrong type parameter)
        // also we wouldn't want to use e.g. `impl S<u32>`

        let same_ty = match blk.self_ty(db).as_adt() {
            Some(def) => def == struct_def,
            None => false,
        };
        let not_trait_impl = blk.trait_(db).is_none();

        if !(same_ty && not_trait_impl) {
            None
        } else {
            Some(impl_blk)
        }
    });

    if let Some(ref impl_blk) = block {
        if has_fn(impl_blk, name) {
            return None;
        }
    }

    Some(block)
}

fn has_fn(imp: &ast::Impl, rhs_name: &str) -> bool {
    if let Some(il) = imp.assoc_item_list() {
        for item in il.assoc_items() {
            if let ast::AssocItem::Fn(f) = item {
                if let Some(name) = f.name() {
                    if name.text().eq_ignore_ascii_case(rhs_name) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Find the start of the `impl` block for the given `ast::Impl`.
//
// FIXME: this partially overlaps with `find_struct_impl`
pub(crate) fn find_impl_block_start(impl_def: ast::Impl, buf: &mut String) -> Option<TextSize> {
    buf.push('\n');
    let start = impl_def.assoc_item_list().and_then(|it| it.l_curly_token())?.text_range().end();
    Some(start)
}

/// Find the end of the `impl` block for the given `ast::Impl`.
//
// FIXME: this partially overlaps with `find_struct_impl`
pub(crate) fn find_impl_block_end(impl_def: ast::Impl, buf: &mut String) -> Option<TextSize> {
    buf.push('\n');
    let end = impl_def
        .assoc_item_list()
        .and_then(|it| it.r_curly_token())?
        .prev_sibling_or_token()?
        .text_range()
        .end();
    Some(end)
}

// Generates the surrounding `impl Type { <code> }` including type and lifetime
// parameters
pub(crate) fn generate_impl_text(adt: &ast::Adt, code: &str) -> String {
    generate_impl_text_inner(adt, None, code)
}

// Generates the surrounding `impl <trait> for Type { <code> }` including type
// and lifetime parameters
pub(crate) fn generate_trait_impl_text(adt: &ast::Adt, trait_text: &str, code: &str) -> String {
    generate_impl_text_inner(adt, Some(trait_text), code)
}

fn generate_impl_text_inner(adt: &ast::Adt, trait_text: Option<&str>, code: &str) -> String {
    let generic_params = adt.generic_param_list();
    let mut buf = String::with_capacity(code.len());
    buf.push_str("\n\n");
    adt.attrs()
        .filter(|attr| attr.as_simple_call().map(|(name, _arg)| name == "cfg").unwrap_or(false))
        .for_each(|attr| buf.push_str(format!("{}\n", attr.to_string()).as_str()));
    buf.push_str("impl");
    if let Some(generic_params) = &generic_params {
        let lifetimes = generic_params.lifetime_params().map(|lt| format!("{}", lt.syntax()));
        let type_params = generic_params.type_params().map(|type_param| {
            let mut buf = String::new();
            if let Some(it) = type_param.name() {
                format_to!(buf, "{}", it.syntax());
            }
            if let Some(it) = type_param.colon_token() {
                format_to!(buf, "{} ", it);
            }
            if let Some(it) = type_param.type_bound_list() {
                format_to!(buf, "{}", it.syntax());
            }
            buf
        });
        let const_params = generic_params.const_params().map(|t| t.syntax().to_string());
        let generics = lifetimes.chain(type_params).chain(const_params).format(", ");
        format_to!(buf, "<{}>", generics);
    }
    buf.push(' ');
    if let Some(trait_text) = trait_text {
        buf.push_str(trait_text);
        buf.push_str(" for ");
    }
    buf.push_str(&adt.name().unwrap().text());
    if let Some(generic_params) = generic_params {
        let lifetime_params = generic_params
            .lifetime_params()
            .filter_map(|it| it.lifetime())
            .map(|it| SmolStr::from(it.text()));
        let type_params = generic_params
            .type_params()
            .filter_map(|it| it.name())
            .map(|it| SmolStr::from(it.text()));
        let const_params = generic_params
            .const_params()
            .filter_map(|it| it.name())
            .map(|it| SmolStr::from(it.text()));
        format_to!(buf, "<{}>", lifetime_params.chain(type_params).chain(const_params).format(", "))
    }

    match adt.where_clause() {
        Some(where_clause) => {
            format_to!(buf, "\n{}\n{{\n{}\n}}", where_clause, code);
        }
        None => {
            format_to!(buf, " {{\n{}\n}}", code);
        }
    }

    buf
}

pub(crate) fn add_method_to_adt(
    builder: &mut AssistBuilder,
    adt: &ast::Adt,
    impl_def: Option<ast::Impl>,
    method: &str,
) {
    let mut buf = String::with_capacity(method.len() + 2);
    if impl_def.is_some() {
        buf.push('\n');
    }
    buf.push_str(method);

    let start_offset = impl_def
        .and_then(|impl_def| find_impl_block_end(impl_def, &mut buf))
        .unwrap_or_else(|| {
            buf = generate_impl_text(adt, &buf);
            adt.syntax().text_range().end()
        });

    builder.insert(start_offset, buf);
}

pub fn useless_type_special_case(field_name: &str, field_ty: &String) -> Option<(String, String)> {
    if field_ty == "String" {
        cov_mark::hit!(useless_type_special_case);
        return Some(("&str".to_string(), format!("self.{}.as_str()", field_name)));
    }
    if let Some(arg) = ty_ctor(field_ty, "Vec") {
        return Some((format!("&[{}]", arg), format!("self.{}.as_slice()", field_name)));
    }
    if let Some(arg) = ty_ctor(field_ty, "Box") {
        return Some((format!("&{}", arg), format!("self.{}.as_ref()", field_name)));
    }
    if let Some(arg) = ty_ctor(field_ty, "Option") {
        return Some((format!("Option<&{}>", arg), format!("self.{}.as_ref()", field_name)));
    }
    None
}

// FIXME: This should rely on semantic info.
fn ty_ctor(ty: &String, ctor: &str) -> Option<String> {
    let res = ty.to_string().strip_prefix(ctor)?.strip_prefix('<')?.strip_suffix('>')?.to_string();
    Some(res)
}

pub(crate) fn get_methods(items: &ast::AssocItemList) -> Vec<ast::Fn> {
    items
        .assoc_items()
        .flat_map(|i| match i {
            ast::AssocItem::Fn(f) => Some(f),
            _ => None,
        })
        .filter(|f| f.name().is_some())
        .collect()
}

/// Trim(remove leading and trailing whitespace) `initial_range` in `source_file`, return the trimmed range.
pub(crate) fn trimmed_text_range(source_file: &SourceFile, initial_range: TextRange) -> TextRange {
    let mut trimmed_range = initial_range;
    while source_file
        .syntax()
        .token_at_offset(trimmed_range.start())
        .find_map(Whitespace::cast)
        .is_some()
        && trimmed_range.start() < trimmed_range.end()
    {
        let start = trimmed_range.start() + TextSize::from(1);
        trimmed_range = TextRange::new(start, trimmed_range.end());
    }
    while source_file
        .syntax()
        .token_at_offset(trimmed_range.end())
        .find_map(Whitespace::cast)
        .is_some()
        && trimmed_range.start() < trimmed_range.end()
    {
        let end = trimmed_range.end() - TextSize::from(1);
        trimmed_range = TextRange::new(trimmed_range.start(), end);
    }
    trimmed_range
}
