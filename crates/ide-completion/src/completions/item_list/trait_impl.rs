//! Completion for associated items in a trait implementation.
//!
//! This module adds the completion items related to implementing associated
//! items within an `impl Trait for Struct` block. The current context node
//! must be within either a `FN`, `TYPE_ALIAS`, or `CONST` node
//! and an direct child of an `IMPL`.
//!
//! # Examples
//!
//! Considering the following trait `impl`:
//!
//! ```ignore
//! trait SomeTrait {
//!     fn foo();
//! }
//!
//! impl SomeTrait for () {
//!     fn f$0
//! }
//! ```
//!
//! may result in the completion of the following method:
//!
//! ```ignore
//! # trait SomeTrait {
//! #    fn foo();
//! # }
//!
//! impl SomeTrait for () {
//!     fn foo() {}$0
//! }
//! ```

use hir::{self, HasAttrs};
use ide_db::{
    path_transform::PathTransform, syntax_helpers::insert_whitespace_into_node,
    traits::get_missing_assoc_items, SymbolKind,
};
use syntax::{
    ast::{self, edit_in_place::AttrsOwnerEdit},
    AstNode, SyntaxElement, SyntaxKind, SyntaxNode, TextRange, T,
};
use text_edit::TextEdit;

use crate::{
    context::PathCompletionCtx, CompletionContext, CompletionItem, CompletionItemKind,
    CompletionRelevance, Completions,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ImplCompletionKind {
    All,
    Fn,
    TypeAlias,
    Const,
}

pub(crate) fn complete_trait_impl_const(
    acc: &mut Completions,
    ctx: &CompletionContext,
    name: &Option<ast::Name>,
) -> Option<()> {
    complete_trait_impl_name(acc, ctx, name, ImplCompletionKind::Const)
}

pub(crate) fn complete_trait_impl_type_alias(
    acc: &mut Completions,
    ctx: &CompletionContext,
    name: &Option<ast::Name>,
) -> Option<()> {
    complete_trait_impl_name(acc, ctx, name, ImplCompletionKind::TypeAlias)
}

pub(crate) fn complete_trait_impl_fn(
    acc: &mut Completions,
    ctx: &CompletionContext,
    name: &Option<ast::Name>,
) -> Option<()> {
    complete_trait_impl_name(acc, ctx, name, ImplCompletionKind::Fn)
}

fn complete_trait_impl_name(
    acc: &mut Completions,
    ctx: &CompletionContext,
    name: &Option<ast::Name>,
    kind: ImplCompletionKind,
) -> Option<()> {
    let token = ctx.token.clone();
    let item = match name {
        Some(name) => name.syntax().parent(),
        None => if token.kind() == SyntaxKind::WHITESPACE { token.prev_token()? } else { token }
            .parent(),
    }?;
    complete_trait_impl(
        acc,
        ctx,
        kind,
        replacement_range(ctx, &item),
        // item -> ASSOC_ITEM_LIST -> IMPL
        &ast::Impl::cast(item.parent()?.parent()?)?,
    );
    Some(())
}

pub(crate) fn complete_trait_impl_item_by_name(
    acc: &mut Completions,
    ctx: &CompletionContext,
    path_ctx: &PathCompletionCtx,
    name_ref: &Option<ast::NameRef>,
    impl_: &Option<ast::Impl>,
) {
    if !path_ctx.is_trivial_path() {
        return;
    }
    if let Some(impl_) = impl_ {
        complete_trait_impl(
            acc,
            ctx,
            ImplCompletionKind::All,
            match name_ref {
                Some(name) => name.syntax().text_range(),
                None => ctx.source_range(),
            },
            impl_,
        );
    }
}

fn complete_trait_impl(
    acc: &mut Completions,
    ctx: &CompletionContext,
    kind: ImplCompletionKind,
    replacement_range: TextRange,
    impl_def: &ast::Impl,
) {
    if let Some(hir_impl) = ctx.sema.to_def(impl_def) {
        get_missing_assoc_items(&ctx.sema, impl_def).into_iter().for_each(|item| {
            use self::ImplCompletionKind::*;
            match (item, kind) {
                (hir::AssocItem::Function(func), All | Fn) => {
                    add_function_impl(acc, ctx, replacement_range, func, hir_impl)
                }
                (hir::AssocItem::TypeAlias(type_alias), All | TypeAlias) => {
                    add_type_alias_impl(acc, ctx, replacement_range, type_alias)
                }
                (hir::AssocItem::Const(const_), All | Const) => {
                    add_const_impl(acc, ctx, replacement_range, const_, hir_impl)
                }
                _ => {}
            }
        });
    }
}

fn add_function_impl(
    acc: &mut Completions,
    ctx: &CompletionContext,
    replacement_range: TextRange,
    func: hir::Function,
    impl_def: hir::Impl,
) {
    let fn_name = func.name(ctx.db);

    let label = format!(
        "fn {}({})",
        fn_name,
        if func.assoc_fn_params(ctx.db).is_empty() { "" } else { ".." }
    );

    let completion_kind = if func.has_self_param(ctx.db) {
        CompletionItemKind::Method
    } else {
        CompletionItemKind::SymbolKind(SymbolKind::Function)
    };

    let mut item = CompletionItem::new(completion_kind, replacement_range, label);
    item.lookup_by(format!("fn {}", fn_name))
        .set_documentation(func.docs(ctx.db))
        .set_relevance(CompletionRelevance { is_item_from_trait: true, ..Default::default() });

    if let Some(source) = ctx.sema.source(func) {
        let assoc_item = ast::AssocItem::Fn(source.value);
        if let Some(transformed_item) = get_transformed_assoc_item(ctx, assoc_item, impl_def) {
            let transformed_fn = match transformed_item {
                ast::AssocItem::Fn(func) => func,
                _ => unreachable!(),
            };

            let function_decl = function_declaration(&transformed_fn, source.file_id.is_macro());
            match ctx.config.snippet_cap {
                Some(cap) => {
                    let snippet = format!("{} {{\n    $0\n}}", function_decl);
                    item.snippet_edit(cap, TextEdit::replace(replacement_range, snippet));
                }
                None => {
                    let header = format!("{} {{", function_decl);
                    item.text_edit(TextEdit::replace(replacement_range, header));
                }
            };
            item.add_to(acc);
        }
    }
}

/// Transform a relevant associated item to inline generics from the impl, remove attrs and docs, etc.
fn get_transformed_assoc_item(
    ctx: &CompletionContext,
    assoc_item: ast::AssocItem,
    impl_def: hir::Impl,
) -> Option<ast::AssocItem> {
    let assoc_item = assoc_item.clone_for_update();
    let trait_ = impl_def.trait_(ctx.db)?;
    let source_scope = &ctx.sema.scope_for_def(trait_);
    let target_scope = &ctx.sema.scope(ctx.sema.source(impl_def)?.syntax().value)?;
    let transform = PathTransform::trait_impl(
        target_scope,
        source_scope,
        trait_,
        ctx.sema.source(impl_def)?.value,
    );

    transform.apply(assoc_item.syntax());
    if let ast::AssocItem::Fn(func) = &assoc_item {
        func.remove_attrs_and_docs();
    }
    Some(assoc_item)
}

fn add_type_alias_impl(
    acc: &mut Completions,
    ctx: &CompletionContext,
    replacement_range: TextRange,
    type_alias: hir::TypeAlias,
) {
    let alias_name = type_alias.name(ctx.db);
    let (alias_name, escaped_name) = (alias_name.to_smol_str(), alias_name.escaped().to_smol_str());

    let label = format!("type {} =", alias_name);
    let replacement = format!("type {} = ", escaped_name);

    let mut item = CompletionItem::new(SymbolKind::TypeAlias, replacement_range, label);
    item.lookup_by(format!("type {}", alias_name))
        .set_documentation(type_alias.docs(ctx.db))
        .set_relevance(CompletionRelevance { is_item_from_trait: true, ..Default::default() });
    match ctx.config.snippet_cap {
        Some(cap) => item
            .snippet_edit(cap, TextEdit::replace(replacement_range, format!("{}$0;", replacement))),
        None => item.text_edit(TextEdit::replace(replacement_range, replacement)),
    };
    item.add_to(acc);
}

fn add_const_impl(
    acc: &mut Completions,
    ctx: &CompletionContext,
    replacement_range: TextRange,
    const_: hir::Const,
    impl_def: hir::Impl,
) {
    let const_name = const_.name(ctx.db).map(|n| n.to_smol_str());

    if let Some(const_name) = const_name {
        if let Some(source) = ctx.sema.source(const_) {
            let assoc_item = ast::AssocItem::Const(source.value);
            if let Some(transformed_item) = get_transformed_assoc_item(ctx, assoc_item, impl_def) {
                let transformed_const = match transformed_item {
                    ast::AssocItem::Const(const_) => const_,
                    _ => unreachable!(),
                };

                let label = make_const_compl_syntax(&transformed_const, source.file_id.is_macro());
                let replacement = format!("{} ", label);

                let mut item = CompletionItem::new(SymbolKind::Const, replacement_range, label);
                item.lookup_by(format!("const {}", const_name))
                    .set_documentation(const_.docs(ctx.db))
                    .set_relevance(CompletionRelevance {
                        is_item_from_trait: true,
                        ..Default::default()
                    });
                match ctx.config.snippet_cap {
                    Some(cap) => item.snippet_edit(
                        cap,
                        TextEdit::replace(replacement_range, format!("{}$0;", replacement)),
                    ),
                    None => item.text_edit(TextEdit::replace(replacement_range, replacement)),
                };
                item.add_to(acc);
            }
        }
    }
}

fn make_const_compl_syntax(const_: &ast::Const, needs_whitespace: bool) -> String {
    const_.remove_attrs_and_docs();
    let const_ = if needs_whitespace {
        insert_whitespace_into_node::insert_ws_into(const_.syntax().clone())
    } else {
        const_.syntax().clone()
    };

    let start = const_.text_range().start();
    let const_end = const_.text_range().end();

    let end = const_
        .children_with_tokens()
        .find(|s| s.kind() == T![;] || s.kind() == T![=])
        .map_or(const_end, |f| f.text_range().start());

    let len = end - start;
    let range = TextRange::new(0.into(), len);

    let syntax = const_.text().slice(range).to_string();

    format!("{} =", syntax.trim_end())
}

fn function_declaration(node: &ast::Fn, needs_whitespace: bool) -> String {
    node.remove_attrs_and_docs();

    let node = if needs_whitespace {
        insert_whitespace_into_node::insert_ws_into(node.syntax().clone())
    } else {
        node.syntax().clone()
    };

    let start = node.text_range().start();
    let end = node.text_range().end();

    let end = node
        .last_child_or_token()
        .filter(|s| s.kind() == T![;] || s.kind() == SyntaxKind::BLOCK_EXPR)
        .map_or(end, |f| f.text_range().start());

    let len = end - start;
    let range = TextRange::new(0.into(), len);

    let syntax = node.text().slice(range).to_string();

    syntax.trim_end().to_owned()
}

fn replacement_range(ctx: &CompletionContext, item: &SyntaxNode) -> TextRange {
    let first_child = item
        .children_with_tokens()
        .find(|child| {
            !matches!(child.kind(), SyntaxKind::COMMENT | SyntaxKind::WHITESPACE | SyntaxKind::ATTR)
        })
        .unwrap_or_else(|| SyntaxElement::Node(item.clone()));

    TextRange::new(first_child.text_range().start(), ctx.source_range().end())
}

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};

    use crate::tests::{check_edit, completion_list_no_kw};

    fn check(ra_fixture: &str, expect: Expect) {
        let actual = completion_list_no_kw(ra_fixture);
        expect.assert_eq(&actual)
    }

    #[test]
    fn no_completion_inside_fn() {
        check(
            r"
trait Test { fn test(); fn test2(); }
struct T;

impl Test for T {
    fn test() {
        t$0
    }
}
",
            expect![[r#"
                sp Self
                st T
                tt Test
                bt u32
            "#]],
        );

        check(
            r"
trait Test { fn test(); fn test2(); }
struct T;

impl Test for T {
    fn test() {
        fn t$0
    }
}
",
            expect![[""]],
        );

        check(
            r"
trait Test { fn test(); fn test2(); }
struct T;

impl Test for T {
    fn test() {
        fn $0
    }
}
",
            expect![[""]],
        );

        // https://github.com/rust-lang/rust-analyzer/pull/5976#issuecomment-692332191
        check(
            r"
trait Test { fn test(); fn test2(); }
struct T;

impl Test for T {
    fn test() {
        foo.$0
    }
}
",
            expect![[r#""#]],
        );

        check(
            r"
trait Test { fn test(_: i32); fn test2(); }
struct T;

impl Test for T {
    fn test(t$0)
}
",
            expect![[r#"
                sp Self
                st T
                bn &mut self
                bn &self
                bn mut self
                bn self
            "#]],
        );

        check(
            r"
trait Test { fn test(_: fn()); fn test2(); }
struct T;

impl Test for T {
    fn test(f: fn $0)
}
",
            expect![[r#"
                sp Self
                st T
            "#]],
        );
    }

    #[test]
    fn no_completion_inside_const() {
        check(
            r"
trait Test { const TEST: fn(); const TEST2: u32; type Test; fn test(); }
struct T;

impl Test for T {
    const TEST: fn $0
}
",
            expect![[r#""#]],
        );

        check(
            r"
trait Test { const TEST: u32; const TEST2: u32; type Test; fn test(); }
struct T;

impl Test for T {
    const TEST: T$0
}
",
            expect![[r#"
                sp Self
                st T
                tt Test
                bt u32
            "#]],
        );

        check(
            r"
trait Test { const TEST: u32; const TEST2: u32; type Test; fn test(); }
struct T;

impl Test for T {
    const TEST: u32 = f$0
}
",
            expect![[r#"
                sp Self
                st T
                tt Test
                bt u32
            "#]],
        );

        check(
            r"
trait Test { const TEST: u32; const TEST2: u32; type Test; fn test(); }
struct T;

impl Test for T {
    const TEST: u32 = {
        t$0
    };
}
",
            expect![[r#"
                sp Self
                st T
                tt Test
                bt u32
            "#]],
        );

        check(
            r"
trait Test { const TEST: u32; const TEST2: u32; type Test; fn test(); }
struct T;

impl Test for T {
    const TEST: u32 = {
        fn $0
    };
}
",
            expect![[""]],
        );

        check(
            r"
trait Test { const TEST: u32; const TEST2: u32; type Test; fn test(); }
struct T;

impl Test for T {
    const TEST: u32 = {
        fn t$0
    };
}
",
            expect![[""]],
        );
    }

    #[test]
    fn no_completion_inside_type() {
        check(
            r"
trait Test { type Test; type Test2; fn test(); }
struct T;

impl Test for T {
    type Test = T$0;
}
",
            expect![[r#"
                sp Self
                st T
                tt Test
                bt u32
            "#]],
        );

        check(
            r"
trait Test { type Test; type Test2; fn test(); }
struct T;

impl Test for T {
    type Test = fn $0;
}
",
            expect![[r#""#]],
        );
    }

    #[test]
    fn name_ref_single_function() {
        check_edit(
            "fn test",
            r#"
trait Test {
    fn test();
}
struct T;

impl Test for T {
    t$0
}
"#,
            r#"
trait Test {
    fn test();
}
struct T;

impl Test for T {
    fn test() {
    $0
}
}
"#,
        );
    }

    #[test]
    fn single_function() {
        check_edit(
            "fn test",
            r#"
trait Test {
    fn test();
}
struct T;

impl Test for T {
    fn t$0
}
"#,
            r#"
trait Test {
    fn test();
}
struct T;

impl Test for T {
    fn test() {
    $0
}
}
"#,
        );
    }

    #[test]
    fn generic_fn() {
        check_edit(
            "fn foo",
            r#"
trait Test {
    fn foo<T>();
}
struct T;

impl Test for T {
    fn f$0
}
"#,
            r#"
trait Test {
    fn foo<T>();
}
struct T;

impl Test for T {
    fn foo<T>() {
    $0
}
}
"#,
        );
        check_edit(
            "fn foo",
            r#"
trait Test {
    fn foo<T>() where T: Into<String>;
}
struct T;

impl Test for T {
    fn f$0
}
"#,
            r#"
trait Test {
    fn foo<T>() where T: Into<String>;
}
struct T;

impl Test for T {
    fn foo<T>() where T: Into<String> {
    $0
}
}
"#,
        );
    }

    #[test]
    fn associated_type() {
        check_edit(
            "type SomeType",
            r#"
trait Test {
    type SomeType;
}

impl Test for () {
    type S$0
}
"#,
            "
trait Test {
    type SomeType;
}

impl Test for () {
    type SomeType = $0;\n\
}
",
        );
        check_edit(
            "type SomeType",
            r#"
trait Test {
    type SomeType;
}

impl Test for () {
    type$0
}
"#,
            "
trait Test {
    type SomeType;
}

impl Test for () {
    type SomeType = $0;\n\
}
",
        );
    }

    #[test]
    fn associated_const() {
        check_edit(
            "const SOME_CONST",
            r#"
trait Test {
    const SOME_CONST: u16;
}

impl Test for () {
    const S$0
}
"#,
            "
trait Test {
    const SOME_CONST: u16;
}

impl Test for () {
    const SOME_CONST: u16 = $0;\n\
}
",
        );

        check_edit(
            "const SOME_CONST",
            r#"
trait Test {
    const SOME_CONST: u16 = 92;
}

impl Test for () {
    const S$0
}
"#,
            "
trait Test {
    const SOME_CONST: u16 = 92;
}

impl Test for () {
    const SOME_CONST: u16 = $0;\n\
}
",
        );
    }

    #[test]
    fn complete_without_name() {
        let test = |completion: &str, hint: &str, completed: &str, next_sibling: &str| {
            check_edit(
                completion,
                &format!(
                    r#"
trait Test {{
    type Foo;
    const CONST: u16;
    fn bar();
}}
struct T;

impl Test for T {{
    {}
    {}
}}
"#,
                    hint, next_sibling
                ),
                &format!(
                    r#"
trait Test {{
    type Foo;
    const CONST: u16;
    fn bar();
}}
struct T;

impl Test for T {{
    {}
    {}
}}
"#,
                    completed, next_sibling
                ),
            )
        };

        // Enumerate some possible next siblings.
        for next_sibling in &[
            "",
            "fn other_fn() {}", // `const $0 fn` -> `const fn`
            "type OtherType = i32;",
            "const OTHER_CONST: i32 = 0;",
            "async fn other_fn() {}",
            "unsafe fn other_fn() {}",
            "default fn other_fn() {}",
            "default type OtherType = i32;",
            "default const OTHER_CONST: i32 = 0;",
        ] {
            test("fn bar", "fn $0", "fn bar() {\n    $0\n}", next_sibling);
            test("type Foo", "type $0", "type Foo = $0;", next_sibling);
            test("const CONST", "const $0", "const CONST: u16 = $0;", next_sibling);
        }
    }

    #[test]
    fn snippet_does_not_overwrite_comment_or_attr() {
        let test = |completion: &str, hint: &str, completed: &str| {
            check_edit(
                completion,
                &format!(
                    r#"
trait Foo {{
    type Type;
    fn function();
    const CONST: i32 = 0;
}}
struct T;

impl Foo for T {{
    // Comment
    #[bar]
    {}
}}
"#,
                    hint
                ),
                &format!(
                    r#"
trait Foo {{
    type Type;
    fn function();
    const CONST: i32 = 0;
}}
struct T;

impl Foo for T {{
    // Comment
    #[bar]
    {}
}}
"#,
                    completed
                ),
            )
        };
        test("fn function", "fn f$0", "fn function() {\n    $0\n}");
        test("type Type", "type T$0", "type Type = $0;");
        test("const CONST", "const C$0", "const CONST: i32 = $0;");
    }

    #[test]
    fn generics_are_inlined_in_return_type() {
        check_edit(
            "fn function",
            r#"
trait Foo<T> {
    fn function() -> T;
}
struct Bar;

impl Foo<u32> for Bar {
    fn f$0
}
"#,
            r#"
trait Foo<T> {
    fn function() -> T;
}
struct Bar;

impl Foo<u32> for Bar {
    fn function() -> u32 {
    $0
}
}
"#,
        )
    }

    #[test]
    fn generics_are_inlined_in_parameter() {
        check_edit(
            "fn function",
            r#"
trait Foo<T> {
    fn function(bar: T);
}
struct Bar;

impl Foo<u32> for Bar {
    fn f$0
}
"#,
            r#"
trait Foo<T> {
    fn function(bar: T);
}
struct Bar;

impl Foo<u32> for Bar {
    fn function(bar: u32) {
    $0
}
}
"#,
        )
    }

    #[test]
    fn generics_are_inlined_when_part_of_other_types() {
        check_edit(
            "fn function",
            r#"
trait Foo<T> {
    fn function(bar: Vec<T>);
}
struct Bar;

impl Foo<u32> for Bar {
    fn f$0
}
"#,
            r#"
trait Foo<T> {
    fn function(bar: Vec<T>);
}
struct Bar;

impl Foo<u32> for Bar {
    fn function(bar: Vec<u32>) {
    $0
}
}
"#,
        )
    }

    #[test]
    fn generics_are_inlined_complex() {
        check_edit(
            "fn function",
            r#"
trait Foo<T, U, V> {
    fn function(bar: Vec<T>, baz: U) -> Arc<Vec<V>>;
}
struct Bar;

impl Foo<u32, Vec<usize>, u8> for Bar {
    fn f$0
}
"#,
            r#"
trait Foo<T, U, V> {
    fn function(bar: Vec<T>, baz: U) -> Arc<Vec<V>>;
}
struct Bar;

impl Foo<u32, Vec<usize>, u8> for Bar {
    fn function(bar: Vec<u32>, baz: Vec<usize>) -> Arc<Vec<u8>> {
    $0
}
}
"#,
        )
    }

    #[test]
    fn generics_are_inlined_in_associated_const() {
        check_edit(
            "const BAR",
            r#"
trait Foo<T> {
    const BAR: T;
}
struct Bar;

impl Foo<u32> for Bar {
    const B$0
}
"#,
            r#"
trait Foo<T> {
    const BAR: T;
}
struct Bar;

impl Foo<u32> for Bar {
    const BAR: u32 = $0;
}
"#,
        )
    }

    #[test]
    fn generics_are_inlined_in_where_clause() {
        check_edit(
            "fn function",
            r#"
trait SomeTrait<T> {}

trait Foo<T> {
    fn function()
        where Self: SomeTrait<T>;
}
struct Bar;

impl Foo<u32> for Bar {
    fn f$0
}
"#,
            r#"
trait SomeTrait<T> {}

trait Foo<T> {
    fn function()
        where Self: SomeTrait<T>;
}
struct Bar;

impl Foo<u32> for Bar {
    fn function()
        where Self: SomeTrait<u32> {
    $0
}
}
"#,
        )
    }

    #[test]
    fn works_directly_in_impl() {
        check(
            r#"
trait Tr {
    fn required();
}

impl Tr for () {
    $0
}
"#,
            expect![[r#"
            fn fn required()
        "#]],
        );
        check(
            r#"
trait Tr {
    fn provided() {}
    fn required();
}

impl Tr for () {
    fn provided() {}
    $0
}
"#,
            expect![[r#"
            fn fn required()
        "#]],
        );
    }

    #[test]
    fn fixes_up_macro_generated() {
        check_edit(
            "fn foo",
            r#"
macro_rules! noop {
    ($($item: item)*) => {
        $($item)*
    }
}

noop! {
    trait Foo {
        fn foo(&mut self, bar: i64, baz: &mut u32) -> Result<(), u32>;
    }
}

struct Test;

impl Foo for Test {
    $0
}
"#,
            r#"
macro_rules! noop {
    ($($item: item)*) => {
        $($item)*
    }
}

noop! {
    trait Foo {
        fn foo(&mut self, bar: i64, baz: &mut u32) -> Result<(), u32>;
    }
}

struct Test;

impl Foo for Test {
    fn foo(&mut self,bar:i64,baz: &mut u32) -> Result<(),u32> {
    $0
}
}
"#,
        );
    }
}
