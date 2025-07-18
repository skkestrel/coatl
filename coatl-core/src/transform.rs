use std::borrow::Cow;

use crate::{
    linecol::LineColCache,
    py::{ast::*, util::PyAstBuilder},
};
use parser::ast::*;

#[derive(Debug)]
pub struct TfErr {
    pub message: String,
    pub span: Option<Span>,
}

#[derive(Debug)]
pub struct TfErrs(pub Vec<TfErr>);

impl TfErrs {
    pub fn new() -> Self {
        TfErrs(vec![])
    }

    pub fn extend(&mut self, other: TfErrs) {
        self.0.extend(other.0);
    }
}

#[derive(Default)]
pub struct TfErrBuilder {
    message: String,
    span: Option<Span>,
}

impl TfErrBuilder {
    pub fn span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn message<S: Into<String>>(mut self, message: S) -> Self {
        self.message = message.into();
        self
    }

    pub fn build(self) -> TfErr {
        TfErr {
            message: self.message,
            span: self.span,
        }
    }

    pub fn build_errs(self) -> TfErrs {
        TfErrs(vec![self.build()])
    }
}

pub type TfResult<T> = Result<T, TfErrs>;

#[allow(dead_code)]
struct TfCtx<'src> {
    source: &'src str,
    exports: Vec<PyIdent<'src>>,
    module_star_exports: Vec<PyIdent<'src>>,

    line_cache: LineColCache,
    placeholder_ctx_stack: Vec<PlaceholderCtx>,
}

impl<'src> TfCtx<'src> {
    fn new(source: &'src str) -> TfResult<Self> {
        Ok(TfCtx {
            source,
            line_cache: LineColCache::new(source),
            exports: Vec::new(),
            module_star_exports: Vec::new(),
            placeholder_ctx_stack: Vec::new(),
        })
    }

    fn linecol(&self, cursor: usize) -> (usize, usize) {
        self.line_cache.linecol(cursor)
    }

    fn temp_var_name(&self, typ: &str, cursor: usize) -> String {
        let (line, col) = self.linecol(cursor);
        format!("__tl_{}_l{}c{}", typ, line, col)
    }
}

enum BlockFinal<'src> {
    Expr(SPyExpr<'src>),
    Never,
    Nothing,
}

struct PyBlockWithFinal<'src> {
    stmts: PyBlock<'src>,
    final_: BlockFinal<'src>,
}

struct PyExprWithPre<'src> {
    pre_stmts: PyBlock<'src>,
    expr: SPyExpr<'src>,
}

trait SBlockExt<'src> {
    fn transform_with_final_stmt<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyBlock<'src>>;

    fn transform_with_final_expr<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyBlockWithFinal<'src>>;

    fn transform<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
        treat_final_as_expr: bool,
        is_top_level: bool,
    ) -> TfResult<PyBlockWithFinal<'src>>;
}

impl<'src> SBlockExt<'src> for SBlock<'src> {
    fn transform_with_final_stmt<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyBlock<'src>> {
        let tr = self.transform(ctx, false, false)?;

        if let BlockFinal::Expr(..) = tr.final_ {
            panic!("there shouldn't be a final expr");
        }

        Ok(tr.stmts)
    }

    fn transform_with_final_expr<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyBlockWithFinal<'src>> {
        Ok(self.transform(ctx, true, false)?)
    }

    fn transform<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
        treat_final_as_expr: bool,
        is_top_level: bool,
    ) -> TfResult<PyBlockWithFinal<'src>> {
        let (block, _span) = self;

        match block {
            Block::Stmts(stmts) => {
                if stmts.is_empty() {
                    return Ok(PyBlockWithFinal {
                        stmts: PyBlock::new(),
                        final_: BlockFinal::Nothing,
                    });
                }

                let mut py_stmts = PyBlock::new();
                let mut errs = Vec::new();
                let mut ok = true;

                let mut handle_stmt = |stmt: &SStmt<'src>| {
                    match stmt.transform(ctx, is_top_level) {
                        Ok(transformed) => {
                            py_stmts.extend(transformed);
                        }
                        Err(e) => {
                            errs.extend(e.0);
                            ok = false;
                        }
                    };
                };

                let mut iter = stmts.iter();

                for stmt in iter.by_ref().take(stmts.len() - 1) {
                    handle_stmt(stmt);
                }

                let final_stmt = iter.next().unwrap();
                let mut final_ = BlockFinal::Nothing;

                if treat_final_as_expr {
                    match &final_stmt.0 {
                        Stmt::Expr(expr, modifiers) => {
                            if !modifiers.is_empty() {
                                return Err(TfErrBuilder::default()
                                    .message("Modifiers are not allowed on expression statements")
                                    .span(final_stmt.1)
                                    .build_errs());
                            }

                            match expr.transform_with_placeholder_guard(ctx) {
                                Ok(expr_with_aux) => {
                                    py_stmts.extend(expr_with_aux.pre_stmts);
                                    final_ = BlockFinal::Expr(expr_with_aux.expr);
                                }
                                Err(e) => {
                                    errs.extend(e.0);
                                    ok = false;
                                }
                            }
                        }
                        _ => {
                            match &final_stmt.0 {
                                Stmt::Return(..)
                                | Stmt::Raise(..)
                                | Stmt::Continue
                                | Stmt::Break => {
                                    final_ = BlockFinal::Never;
                                }
                                _ => {}
                            }

                            handle_stmt(final_stmt);
                        }
                    }
                } else {
                    handle_stmt(final_stmt);
                }

                if ok {
                    Ok(PyBlockWithFinal {
                        stmts: py_stmts,
                        final_,
                    })
                } else {
                    Err(TfErrs(errs))
                }
            }
            Block::Expr(sexpr) => {
                if treat_final_as_expr {
                    let t = sexpr.transform(ctx)?;

                    Ok(PyBlockWithFinal {
                        stmts: t.pre_stmts,
                        final_: BlockFinal::Expr(t.expr),
                    })
                } else {
                    // TODO this clone is bad - refactor AST ownership?

                    let f = (Stmt::Expr(sexpr.clone(), vec![]), sexpr.1);
                    let stmts = f.transform(ctx, is_top_level)?;

                    Ok(PyBlockWithFinal {
                        stmts,
                        final_: BlockFinal::Nothing,
                    })
                }
            }
        }
    }
}

fn destructure_list<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    target: &'ast SExpr<'src>,
    items: &'ast [ListItem<'src>],
    decl_only: bool,
) -> TfResult<DestructureBindings<'src>> {
    let cursor_var = ctx.temp_var_name("des_curs", target.1.start);
    let list_var = ctx.temp_var_name("des_list", target.1.start);
    let len_var = ctx.temp_var_name("des_len", target.1.start);

    // list_var = list(cursor_var)
    // len_var = len(list_var)

    let a = PyAstBuilder::new(target.1);

    let mut stmts = PyBlock(vec![
        a.assign(
            a.ident(list_var.clone(), PyAccessCtx::Store),
            a.call(
                a.load_ident("list"),
                vec![a.call_arg(a.load_ident(cursor_var.clone()))],
            ),
        ),
        a.assign(
            a.ident(len_var.clone(), PyAccessCtx::Store),
            a.call(
                a.load_ident("len"),
                vec![a.call_arg(a.load_ident(list_var.clone()))],
            ),
        ),
    ]);

    let mut post_stmts = vec![];
    let mut decls = vec![];

    // a = list_var[0]
    // b = list_var[1]
    // c = list_var[i:len_var-n_single_spreads]

    let mut seen_spread = false;
    let mut i = 0;

    for item in items.iter() {
        match item {
            ListItem::Item(expr) => {
                let item_bindings = destructure(ctx, expr, decl_only)?;
                post_stmts.extend(item_bindings.post_stmts);
                decls.extend(item_bindings.declarations);

                stmts.push(
                    a.assign(
                        item_bindings.assign_to,
                        a.subscript(
                            a.load_ident(list_var.clone()),
                            a.num(
                                (if seen_spread {
                                    -((items.len() - i - 1) as i32)
                                } else {
                                    i as i32
                                })
                                .to_string(),
                            ),
                            PyAccessCtx::Load,
                        ),
                    ),
                );
                i += 1;
            }
            ListItem::Spread(expr) => {
                if seen_spread {
                    return Err(TfErrBuilder::default()
                        .message("Destructuring assignment with multiple spreads is not allowed")
                        .span(target.1)
                        .build_errs());
                }
                seen_spread = true;

                let item_bindings = destructure(ctx, expr, decl_only)?;
                post_stmts.extend(item_bindings.post_stmts);
                decls.extend(item_bindings.declarations);

                stmts.push(a.assign(
                    item_bindings.assign_to,
                    a.subscript(
                        a.load_ident(list_var.clone()),
                        a.slice(
                            Some(a.num(i.to_string())),
                            Some(a.binary(
                                PyBinaryOp::Sub,
                                a.load_ident(len_var.clone()),
                                a.num((items.len() - 2).to_string()),
                            )),
                            None,
                        ),
                        PyAccessCtx::Load,
                    ),
                ));
            }
        }
    }

    stmts.extend(post_stmts);

    Ok(DestructureBindings {
        post_stmts: stmts,
        assign_to: a.ident(cursor_var, PyAccessCtx::Store),
        declarations: decls,
    })
}

fn destructure_mapping<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    target: &'ast SExpr<'src>,
    items: &'ast [MappingItem<'src>],
    decl_only: bool,
) -> TfResult<DestructureBindings<'src>> {
    let cursor_var = ctx.temp_var_name("des_curs", target.1.start);
    let dict_var = ctx.temp_var_name("des_dict", target.1.start);

    // dict_var = dict(cursor_var)
    let a = PyAstBuilder::new(target.1);
    let mut stmts = PyBlock(vec![a.assign(
        a.ident(dict_var.clone(), PyAccessCtx::Store),
        a.call(
            a.load_ident("dict"),
            vec![a.call_arg(a.load_ident(cursor_var.clone()))],
        ),
    )]);

    let mut post_stmts = vec![];
    let mut decls = vec![];

    // a = dict_var.pop(a_key)
    // b = dict_var.pop(b_key)
    // c = dict_var

    let mut spread_var = None;
    for item in items.iter() {
        match item {
            MappingItem::Item(key, expr) => {
                let item_bindings = destructure(ctx, expr, decl_only)?;
                let key_node = key.transform(ctx)?;
                post_stmts.extend(key_node.pre_stmts);
                post_stmts.extend(item_bindings.post_stmts);
                decls.extend(item_bindings.declarations);

                stmts.push(a.assign(
                    item_bindings.assign_to,
                    a.call(
                        a.attribute(a.load_ident(dict_var.clone()), "pop", PyAccessCtx::Load),
                        vec![a.call_arg(key_node.expr)],
                    ),
                ));
            }
            MappingItem::Spread(expr) => {
                if spread_var.is_some() {
                    return Err(TfErrBuilder::default()
                        .message("Destructuring assignment with multiple spreads is not allowed")
                        .span(target.1)
                        .build_errs());
                }

                spread_var = Some(expr);
            }
        }
    }

    if let Some(spread_var) = spread_var {
        let item_bindings = destructure(ctx, spread_var, decl_only)?;

        post_stmts.extend(item_bindings.post_stmts);
        decls.extend(item_bindings.declarations);

        stmts.push(a.assign(item_bindings.assign_to, a.load_ident(dict_var.clone())));
    }

    stmts.extend(post_stmts);

    Ok(DestructureBindings {
        post_stmts: stmts,
        assign_to: a.ident(cursor_var, PyAccessCtx::Store),
        declarations: decls,
    })
}

struct DestructureBindings<'a> {
    assign_to: SPyExpr<'a>,
    post_stmts: PyBlock<'a>,
    declarations: Vec<PyIdent<'a>>,
}

fn destructure<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    target: &'ast SExpr<'src>,
    decl_only: bool,
) -> TfResult<DestructureBindings<'src>> {
    let mut post_stmts = PyBlock::new();
    let mut decls = Vec::<PyIdent<'src>>::new();

    let assign_to: SPyExpr<'src>;

    match &target.0 {
        Expr::Ident(..) | Expr::Attribute(..) | Expr::Subscript(..) => {
            match &target.0 {
                Expr::Ident(id) => {
                    decls.push(id.0.to_owned().into());
                }
                Expr::Attribute(..) | Expr::Subscript(..) => {
                    if decl_only {
                        return Err(TfErrBuilder::default()
                            .message("Only identifiers allowed in this destructuring")
                            .span(target.1)
                            .build_errs());
                    }
                }
                _ => {
                    panic!();
                }
            }

            let target_node = target.transform_with_access(ctx, PyAccessCtx::Store)?;
            post_stmts.extend(target_node.pre_stmts);

            assign_to = target_node.expr;
        }
        Expr::List(items) => {
            let bindings = destructure_list(ctx, target, items, decl_only)?;

            post_stmts.extend(bindings.post_stmts);
            decls.extend(bindings.declarations);
            assign_to = bindings.assign_to;
        }
        Expr::Mapping(items) => {
            let bindings = destructure_mapping(ctx, target, items, decl_only)?;

            post_stmts.extend(bindings.post_stmts);
            decls.extend(bindings.declarations);
            assign_to = bindings.assign_to;
        }
        _ => {
            return Err(TfErrBuilder::default()
                .message("Assignment target is not allowed")
                .span(target.1)
                .build_errs());
        }
    };

    Ok(DestructureBindings {
        post_stmts,
        assign_to,
        declarations: decls,
    })
}

trait SStmtExt<'src> {
    fn transform<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
        top_level: bool,
    ) -> TfResult<PyBlock<'src>>;
}

fn get_scope_modifier<'a>(
    mods: &'a Vec<AssignModifier>,
    is_top_level: bool,
    span: &Span,
) -> TfResult<Option<&'a AssignModifier>> {
    let scope_modifier = mods
        .iter()
        .filter(|m| {
            matches!(
                m,
                AssignModifier::Export | AssignModifier::Global | AssignModifier::Nonlocal
            )
        })
        .collect::<Vec<_>>();

    if scope_modifier.len() > 1 {
        return Err(TfErrBuilder::default()
            .message("Only one scope modifier is allowed in an assignment")
            .span(*span)
            .build_errs());
    }

    let scope_modifier = scope_modifier.first().map(|x| *x);

    if !is_top_level && scope_modifier.is_some_and(|x| *x == AssignModifier::Export) {
        return Err(TfErrBuilder::default()
            .message("Export modifier is only allowed at the top level")
            .span(*span)
            .build_errs());
    }

    Ok(scope_modifier)
}

fn get_scope_modifying_statements<'a>(
    ctx: &mut TfCtx<'a>,
    scope_modifier: Option<&AssignModifier>,
    decls: Vec<PyIdent<'a>>,
    span: &Span,
) -> TfResult<PyBlock<'a>> {
    let mut stmts = PyBlock::new();

    if let Some(scope_modifier) = scope_modifier {
        match scope_modifier {
            AssignModifier::Export => ctx.exports.extend(decls),
            AssignModifier::Global => {
                // exports are implemented by lifting into global scope

                stmts.push((PyStmt::Global(decls), *span).into());
            }

            AssignModifier::Nonlocal => {
                stmts.push((PyStmt::Nonlocal(decls), *span).into());
            }
        }
    }

    Ok(stmts)
}

fn transform_assignment<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    lhs: &'ast SExpr<'src>,
    rhs: &'ast SExpr<'src>,
    scope_modifier: Option<&AssignModifier>,
    span: &Span,
) -> TfResult<(PyBlock<'src>, Vec<PyIdent<'src>>)> {
    let mut stmts = PyBlock::new();
    if let Expr::Ident((ident, _ident_span)) = &lhs.0 {
        let mut decorators = vec![];
        let mut cur_node = &rhs.0;

        loop {
            match cur_node {
                Expr::Then(left, right) => {
                    cur_node = &left.0;
                    decorators.push(right);
                }
                Expr::Binary(BinaryOp::Pipe, left, right) => {
                    cur_node = &left.0;
                    decorators.push(right);
                }
                Expr::Call(left, right) => {
                    if right.len() != 1 {
                        break;
                    }

                    match &right[0].0 {
                        CallItem::Arg(arg) => {
                            cur_node = &arg.0;
                            decorators.push(left);
                        }
                        _ => {
                            break;
                        }
                    }
                }
                _ => {
                    break;
                }
            }
        }

        let py_decorators = || {
            Ok(PyDecorators(
                decorators
                    .into_iter()
                    .map(|x| {
                        let t = x.transform_with_placeholder_guard(ctx)?;
                        stmts.extend(t.pre_stmts);
                        Ok(t.expr)
                    })
                    .collect::<TfResult<_>>()?,
            ))
        };

        if let Expr::Fn(arglist, body) = &cur_node {
            let decorators = py_decorators()?;
            return Ok((
                make_fn_def(
                    ctx,
                    (*ident).into(),
                    FnDefArgs::ArgList(arglist),
                    FnDefBody::Block(body),
                    decorators,
                    span,
                )?,
                vec![(*ident).into()],
            ));
        } else if let Expr::Class(bases, body) = &cur_node {
            let decorators = py_decorators()?;
            return Ok((
                make_class_def(ctx, (*ident).into(), &bases, &body, decorators, span)?,
                vec![(*ident).into()],
            ));
        };
    };

    let value_node = rhs.transform_with_placeholder_guard(ctx)?;
    stmts.extend(value_node.pre_stmts);

    let decl_only = scope_modifier.is_some();
    let destructure = destructure(ctx, lhs, decl_only)?;

    stmts.push(
        (
            PyStmt::Assign(destructure.assign_to, value_node.expr),
            lhs.1,
        )
            .into(),
    );
    stmts.extend(destructure.post_stmts);

    Ok((stmts, destructure.declarations))
}

impl<'src> SStmtExt<'src> for SStmt<'src> {
    fn transform<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
        top_level: bool,
    ) -> TfResult<PyBlock<'src>> {
        let (stmt, span) = self;

        match &stmt {
            Stmt::Expr(expr, modifiers) => {
                if !modifiers.is_empty() {
                    return Err(TfErrBuilder::default()
                        .message("Modifiers are not allowed on expression statements")
                        .span(*span)
                        .build_errs());
                }

                match &expr.0 {
                    Expr::If(cond, then_block, else_block) => {
                        transform_if_stmt(ctx, cond, then_block, else_block, span)
                    }
                    Expr::Match(subject, cases) => transform_match_stmt(ctx, subject, cases, span),
                    _ => {
                        let expr = expr.transform_with_placeholder_guard(ctx)?;
                        let mut stmts = expr.pre_stmts;
                        stmts.push((PyStmt::Expr(expr.expr), *span).into());

                        Ok(stmts)
                    }
                }
            }
            Stmt::Assert(expr, msg) => {
                let expr_node = expr.transform_with_placeholder_guard(ctx)?;
                let msg = msg
                    .as_ref()
                    .map(|x| x.transform_with_placeholder_guard(ctx))
                    .transpose()?;

                let mut stmts = expr_node.pre_stmts;
                let mut msg_node = None;
                if let Some(msg) = msg {
                    stmts.extend(msg.pre_stmts);
                    msg_node = Some(msg.expr);
                }

                stmts.push((PyStmt::Assert(expr_node.expr, msg_node), *span).into());

                Ok(stmts)
            }
            Stmt::Return(expr) => {
                let value = expr.transform_with_placeholder_guard(ctx)?;
                let mut stmts = value.pre_stmts;

                stmts.push((PyStmt::Return(value.expr), *span).into());

                Ok(stmts)
            }
            Stmt::Assign(target, value, modifiers) => {
                let scope_modifier = get_scope_modifier(modifiers, top_level, span)?;

                let (binding_stmts, decls): (PyBlock, Vec<PyIdent>) =
                    transform_assignment(ctx, target, value, scope_modifier, span)?;

                let mut stmts = PyBlock::new();

                stmts.extend(get_scope_modifying_statements(
                    ctx,
                    scope_modifier,
                    decls,
                    span,
                )?);
                stmts.extend(binding_stmts);

                Ok(stmts)
            }
            Stmt::Raise(expr) => {
                let expr_node = expr.transform_with_placeholder_guard(ctx)?;
                let mut stmts = expr_node.pre_stmts;

                stmts.push((PyStmt::Raise(expr_node.expr), *span).into());

                Ok(stmts)
            }
            Stmt::For(target, iter, body) => {
                let target: &SExpr<'src> = target;
                let iter_node: PyExprWithPre<'src> = iter.transform_with_placeholder_guard(ctx)?;
                let aux_stmts = iter_node.pre_stmts;

                let mut block = aux_stmts;

                let destructure = destructure(ctx, target, true)?;

                let mut body_block = PyBlock::new();
                body_block.extend(destructure.post_stmts);
                body_block.extend(body.transform_with_final_stmt(ctx)?);

                block.push(
                    (
                        PyStmt::For(destructure.assign_to, iter_node.expr, body_block),
                        *span,
                    )
                        .into(),
                );

                Ok(block)
            }
            Stmt::While(cond, body) => {
                let cond_node = cond.transform_with_placeholder_guard(ctx)?;
                let body_block = body.transform_with_final_stmt(ctx)?;

                let mut stmts = PyBlock::new();

                let cond: SPyExpr<'src> = if cond_node.pre_stmts.is_empty() {
                    cond_node.expr
                } else {
                    let aux_fn = make_fn_exp(
                        ctx,
                        FnDefArgs::PyArgList(vec![]),
                        FnDefBody::PyStmts(cond_node.pre_stmts),
                        span,
                    )?;

                    stmts.extend(aux_fn.pre_stmts);

                    (PyExpr::Call(Box::new(aux_fn.expr), vec![]), *span).into()
                };

                stmts.push((PyStmt::While(cond, body_block), *span).into());

                Ok(stmts)
            }
            Stmt::Try(body, excepts, finally) => {
                let body_block = body.transform_with_final_stmt(ctx)?;
                let finally_block = finally
                    .as_ref()
                    .map(|f| f.transform_with_final_stmt(ctx))
                    .transpose()?;

                let mut stmts = PyBlock::new();
                let mut excepts_ast = vec![];

                for except in excepts {
                    let except_types = if let Some(types) = &except.types {
                        types.transform(ctx, span)?
                    } else {
                        (PyExpr::Ident("Exception".into(), PyAccessCtx::Load), *span).into()
                    };

                    let ident_node = if let Some(ident) = &except.name {
                        Some(ident.0.into())
                    } else {
                        None
                    };

                    let body_block = except.body.transform_with_final_stmt(ctx)?;

                    let except_ast = PyExceptHandler {
                        typ: Some(except_types),
                        name: ident_node,
                        body: body_block,
                    };

                    excepts_ast.push(except_ast);
                }

                stmts.push((PyStmt::Try(body_block, excepts_ast, finally_block), *span).into());

                Ok(stmts)
            }
            Stmt::Break => Ok(PyBlock(vec![(PyStmt::Break, *span).into()])),
            Stmt::Continue => Ok(PyBlock(vec![(PyStmt::Continue, *span).into()])),
            Stmt::Import(import_stmt) => {
                let mut aliases = vec![];

                let base_module = import_stmt
                    .trunk
                    .iter()
                    .map(|ident| ident.0.as_ref())
                    .collect::<Vec<_>>()
                    .join(".");

                let full_module = ".".repeat(import_stmt.level) + &base_module;

                if import_stmt.reexport {
                    if !top_level {
                        return Err(TfErrBuilder::default()
                            .message("Re-exporting imports is only allowed at the top level")
                            .span(*span)
                            .build_errs());
                    }
                }

                match &import_stmt.imports {
                    ImportList::Star => {
                        aliases.push(PyImportAlias {
                            name: "*".into(),
                            as_name: None,
                        });

                        if import_stmt.reexport {
                            ctx.module_star_exports.push(full_module.into());
                        }
                    }
                    ImportList::Leaves(imports) => {
                        for (ident, alias) in imports {
                            aliases.push(PyImportAlias {
                                name: ident.0.into(),
                                as_name: alias.as_ref().map(|a| a.0.into()),
                            });
                        }

                        if import_stmt.reexport {
                            // alias else orig_name
                            ctx.exports.extend(
                                aliases
                                    .iter()
                                    .map(|x| x.as_name.clone().unwrap_or(x.name.clone())),
                            );
                        }
                    }
                }

                let a = PyAstBuilder::new(*span);

                let py_import: SPyStmt = if !import_stmt.trunk.is_empty() {
                    a.import_from(Some(base_module.into()), aliases, import_stmt.level)
                } else if import_stmt.level != 0 {
                    a.import_from(None, aliases, import_stmt.level)
                } else {
                    a.import(aliases)
                };

                Ok(PyBlock(vec![py_import]))
            }
            Stmt::Err => Err(TfErrBuilder::default()
                .message("unexpected statement error (should have been caught in lexer)".to_owned())
                .span(*span)
                .build_errs()),
            Stmt::Module => Err(TfErrBuilder::default()
                .message("Module statements are not allowed in the transform phase".to_owned())
                .span(*span)
                .build_errs()),
        }
    }
}

fn transform_if_stmt<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    cond: &'ast SExpr<'src>,
    then_block: &'ast SBlock<'src>,
    else_block: &'ast Option<Box<SBlock<'src>>>,
    span: &Span,
) -> TfResult<PyBlock<'src>> {
    let mut stmts = vec![];

    let cond = cond.transform_with_placeholder_guard(ctx)?;
    stmts.extend(cond.pre_stmts.into_iter());

    let then_block = then_block.transform_with_final_stmt(ctx)?;
    let then_block_ast = then_block;

    let mut else_block_ast = None;
    if let Some(else_block) = else_block {
        let else_block = else_block.transform_with_final_stmt(ctx)?;

        else_block_ast = Some(else_block);
    }

    Ok(PyBlock(vec![
        (PyStmt::If(cond.expr, then_block_ast, else_block_ast), *span).into(),
    ]))
}

fn transform_if_expr<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    cond: &'ast SExpr<'src>,
    then_block: &'ast SBlock<'src>,
    else_block: &'ast Option<Box<SBlock<'src>>>,
    span: &Span,
) -> TfResult<PyExprWithPre<'src>> {
    let cond = cond.transform(ctx)?;
    let mut aux_stmts = cond.pre_stmts;

    let ret_varname = ctx.temp_var_name("ifexp", span.start);
    let store_ret_var: SPyExpr = (
        PyExpr::Ident(ret_varname.clone().into(), PyAccessCtx::Store),
        *span,
    )
        .into();
    let load_ret_var: SPyExpr = (
        PyExpr::Ident(ret_varname.clone().into(), PyAccessCtx::Load),
        *span,
    )
        .into();

    let PyBlockWithFinal { stmts, final_ } = then_block.transform_with_final_expr(ctx)?;
    let mut then_block_ast = stmts;

    if let BlockFinal::Expr(final_expr) = final_ {
        then_block_ast.push((PyStmt::Assign(store_ret_var.clone(), final_expr), *span).into());
    } else if let BlockFinal::Never = final_ {
    } else {
        return Err(TfErrBuilder::default()
            .message("then block must have a final expression")
            .span(then_block.1)
            .build_errs());
    }

    let else_block = else_block.as_ref().ok_or_else(|| {
        TfErrBuilder::default()
            .message("else block is required in an if-expr")
            .span(*span)
            .build_errs()
    })?;

    let PyBlockWithFinal { stmts, final_ } = else_block.transform_with_final_expr(ctx)?;
    let mut else_block_ast = stmts;
    if let BlockFinal::Expr(final_expr) = final_ {
        else_block_ast.push((PyStmt::Assign(store_ret_var, final_expr), *span).into());
    } else if let BlockFinal::Never = final_ {
    } else {
        return Err(TfErrBuilder::default()
            .message("else block must have a final expression")
            .span(else_block.1)
            .build_errs());
    }

    aux_stmts.push(
        (
            PyStmt::If(cond.expr, then_block_ast, Some(else_block_ast)),
            *span,
        )
            .into(),
    );

    Ok(PyExprWithPre {
        expr: load_ret_var,
        pre_stmts: aux_stmts,
    })
}

fn transform_match_stmt<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    subject: &'ast SExpr<'src>,
    cases: &'ast [(Option<SExpr<'src>>, SBlock<'src>)],
    span: &Span,
) -> TfResult<PyBlock<'src>> {
    let subject = subject.transform_with_placeholder_guard(ctx)?;
    let mut aux_stmts = subject.pre_stmts;

    let mut py_cases = vec![];
    for (pattern, block) in cases {
        let pattern = if let Some(pattern) = pattern {
            let t = pattern.transform_with_placeholder_guard(ctx)?;
            aux_stmts.extend(t.pre_stmts);
            t.expr
        } else {
            (PyExpr::Ident("_".into(), PyAccessCtx::Load), *span).into()
        };

        let py_block = block.transform_with_final_stmt(ctx)?;

        py_cases.push(PyMatchCase {
            pattern,
            body: py_block,
        });
    }

    aux_stmts.push((PyStmt::Match(subject.expr, py_cases), *span).into());

    Ok(aux_stmts)
}

fn transform_match_expr<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    subject: &'ast SExpr<'src>,
    cases: &'ast [(Option<SExpr<'src>>, SBlock<'src>)],
    span: &Span,
) -> TfResult<PyExprWithPre<'src>> {
    let subject = subject.transform_with_placeholder_guard(ctx)?;
    let mut aux_stmts = subject.pre_stmts;

    let ret_varname = ctx.temp_var_name("matchexp", span.start);
    let load_ret_var: SPyExpr = (
        PyExpr::Ident(ret_varname.clone().into(), PyAccessCtx::Load),
        *span,
    )
        .into();
    let store_ret_var: SPyExpr = (
        PyExpr::Ident(ret_varname.clone().into(), PyAccessCtx::Store),
        *span,
    )
        .into();

    let mut py_cases = vec![];
    let mut has_default_case = false;

    for (i, (pattern, block)) in cases.iter().enumerate() {
        let pattern = if let Some(pattern) = pattern {
            let t = pattern.transform_with_placeholder_guard(ctx)?;
            aux_stmts.extend(t.pre_stmts);
            t.expr
        } else {
            if i != cases.len() - 1 {
                return Err(TfErrBuilder::default()
                    .message("match-expr default case must be the last case")
                    .span(block.1)
                    .build_errs());
            }

            (PyExpr::Ident("_".into(), PyAccessCtx::Load), *span).into()
        };

        if let PyExpr::Ident(..) = pattern.value {
            has_default_case = true;
        }

        let py_block = block.transform_with_final_expr(ctx)?;
        let mut block_stmts = py_block.stmts;

        if let BlockFinal::Expr(final_expr) = py_block.final_ {
            block_stmts.push((PyStmt::Assign(store_ret_var.clone(), final_expr), block.1).into());
        } else if let BlockFinal::Never = py_block.final_ {
        } else {
            return Err(TfErrBuilder::default()
                .message("match-expr case must have a final expression")
                .span((*block).1)
                .build_errs());
        }

        py_cases.push(PyMatchCase {
            pattern,
            body: block_stmts,
        });
    }

    if !has_default_case {
        return Err(TfErrBuilder::default()
            .message("match-expr must have a default case")
            .span(*span)
            .build_errs());
    }

    aux_stmts.push((PyStmt::Match(subject.expr, py_cases), *span).into());

    Ok(PyExprWithPre {
        expr: load_ret_var,
        pre_stmts: aux_stmts,
    })
}

fn make_class_def<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    name: Cow<'src, str>,
    bases: &'ast Vec<SCallItem<'src>>,
    body: &'ast Box<SBlock<'src>>,
    decorators: PyDecorators<'src>,
    span: &Span,
) -> TfResult<PyBlock<'src>> {
    let mut stmts = PyBlock::new();
    let mut bases_nodes: Vec<PyCallItem<'src>> = vec![];

    let block = body.transform_with_final_stmt(ctx)?;

    for base in bases {
        let call_item: PyCallItem<'src> = match &base.0 {
            CallItem::Arg(expr) => {
                let base_node = expr.transform_with_placeholder_guard(ctx)?;
                stmts.extend(base_node.pre_stmts);
                PyCallItem::Arg(base_node.expr)
            }
            CallItem::Kwarg(name, expr) => {
                let expr_node = expr.transform_with_placeholder_guard(ctx)?;
                stmts.extend(expr_node.pre_stmts);
                PyCallItem::Kwarg(name.0.into(), expr_node.expr)
            }
            _ => {
                return Err(TfErrBuilder::default()
                    .message("spread args are not allowed in class bases")
                    .span(*span)
                    .build_errs());
            }
        };

        bases_nodes.push(call_item);
    }

    stmts.push(
        (
            PyStmt::ClassDef(name.into(), bases_nodes, block, decorators),
            *span,
        )
            .into(),
    );

    Ok(stmts)
}

enum FnDefBody<'src, 'ast> {
    PyStmts(PyBlock<'src>),
    Block(&'ast SBlock<'src>),
}

enum FnDefArgs<'src, 'ast> {
    ArgList(&'ast [ArgDefItem<'src>]),
    PyArgList(Vec<PyArgDefItem<'src>>),
}

fn make_arglist<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    arglist: FnDefArgs<'src, 'ast>,
) -> TfResult<(PyBlock<'src>, PyBlock<'src>, Vec<PyArgDefItem<'src>>)> {
    let mut pre = PyBlock::new();
    let mut post = PyBlock::new();

    let args = match arglist {
        FnDefArgs::ArgList(args) => {
            let mut args_vec = vec![];
            for arg in args {
                let arg = match arg {
                    ArgDefItem::Arg(arg, default) => {
                        let default = if let Some(default) = default {
                            let t = default.transform_with_placeholder_guard(ctx)?;
                            pre.extend(t.pre_stmts);
                            Some(t.expr)
                        } else {
                            None
                        };

                        let des = destructure(ctx, &arg, true)?;
                        post.extend(des.post_stmts);

                        let assign_name = match des.assign_to.value {
                            PyExpr::Ident(ident, _) => ident,
                            _ => {
                                return Err(TfErrBuilder::default()
                                    .message("Internal error: Destructuring assignment target must be an identifier")
                                    .span(arg.1)
                                    .build_errs());
                            }
                        };

                        PyArgDefItem::Arg(assign_name, default)
                    }
                    ArgDefItem::ArgSpread(name) => PyArgDefItem::ArgSpread(name.0.into()),
                    ArgDefItem::KwargSpread(name) => PyArgDefItem::KwargSpread(name.0.into()),
                };
                args_vec.push(arg);
            }
            args_vec
        }
        FnDefArgs::PyArgList(args) => args,
    };

    Ok((pre, post, args))
}

fn prepare_py_fn<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    arglist: FnDefArgs<'src, 'ast>,
    body: FnDefBody<'src, 'ast>,
    span: &Span,
) -> TfResult<(PyBlock<'src>, PyBlock<'src>, Vec<PyArgDefItem<'src>>)> {
    let mut aux_stmts = PyBlock::new();
    let mut body_stmts = PyBlock::new();

    let (pre, post, args) = make_arglist(ctx, arglist)?;
    aux_stmts.extend(pre);
    body_stmts.extend(post);

    body_stmts.extend(match body {
        FnDefBody::PyStmts(stmts) => stmts,
        FnDefBody::Block(block) => {
            let block = block.transform_with_final_expr(ctx)?;
            let mut stmts = block.stmts;

            if let BlockFinal::Expr(final_expr) = block.final_ {
                stmts.push((PyStmt::Return(final_expr), *span).into());
            }

            stmts
        }
    });

    Ok((aux_stmts, body_stmts, args))
}

fn make_fn_exp<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    arglist: FnDefArgs<'src, 'ast>,
    body: FnDefBody<'src, 'ast>,
    span: &Span,
) -> TfResult<PyExprWithPre<'src>> {
    let (mut aux_stmts, body_stmts, args) = prepare_py_fn(ctx, arglist, body, span)?;

    if body_stmts.0.len() == 1 {
        // TODO maybe refactor prepare_py_fn to return body_stmts as PyExprWithPre
        if let PyStmt::Return(expr) = &body_stmts.0[0].value {
            return Ok(PyExprWithPre {
                expr: (PyExpr::Lambda(args, Box::new(expr.clone())), *span).into(),
                pre_stmts: PyBlock::new(),
            });
        }
    }

    let name = ctx.temp_var_name("fnexp", span.start);
    aux_stmts.push(
        (
            PyStmt::FnDef(name.clone().into(), args, body_stmts, PyDecorators::new()),
            *span,
        )
            .into(),
    );
    Ok(PyExprWithPre {
        expr: (PyExpr::Ident(name.into(), PyAccessCtx::Load), *span).into(),
        pre_stmts: aux_stmts,
    })
}

fn make_fn_def<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    name: Cow<'src, str>,
    arglist: FnDefArgs<'src, 'ast>,
    body: FnDefBody<'src, 'ast>,
    decorators: PyDecorators<'src>,
    span: &Span,
) -> TfResult<PyBlock<'src>> {
    let (mut aux_stmts, body_stmts, args) = prepare_py_fn(ctx, arglist, body, span)?;
    aux_stmts.push(
        (
            PyStmt::FnDef(name.into(), args, body_stmts, decorators),
            *span,
        )
            .into(),
    );
    Ok(aux_stmts)
}

fn transform_call_items<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    args: &'ast [SCallItem<'src>],
    span: &Span,
) -> TfResult<(PyBlock<'src>, Vec<PyCallItem<'src>>)> {
    let mut started_kwargs = false;
    let mut call_items = vec![];
    let mut aux_stmts = PyBlock::new();

    for arg in args {
        match &arg.0 {
            CallItem::Arg(expr) => {
                if started_kwargs {
                    return Err(TfErrBuilder::default()
                        .message("Cannot have args after kwargs")
                        .span(*span)
                        .build_errs());
                }

                let e = expr.transform_with_deep_placeholder_guard(ctx)?;
                aux_stmts.extend(e.pre_stmts);
                call_items.push(PyCallItem::Arg(e.expr));
            }
            CallItem::Kwarg((name, _name_span), expr) => {
                started_kwargs = true;
                let e = expr.transform_with_deep_placeholder_guard(ctx)?;
                aux_stmts.extend(e.pre_stmts);
                call_items.push(PyCallItem::Kwarg((*name).into(), e.expr));
            }
            CallItem::ArgSpread(expr) => {
                if started_kwargs {
                    return Err(TfErrBuilder::default()
                        .message("Cannot have arg spread after kwargs")
                        .span(*span)
                        .build_errs());
                }

                let e = expr.transform_with_deep_placeholder_guard(ctx)?;
                aux_stmts.extend(e.pre_stmts);
                call_items.push(PyCallItem::ArgSpread(e.expr));
            }
            CallItem::KwargSpread(expr) => {
                started_kwargs = true;
                let e = expr.transform_with_deep_placeholder_guard(ctx)?;
                aux_stmts.extend(e.pre_stmts);
                call_items.push(PyCallItem::KwargSpread(e.expr));
            }
        };
    }

    Ok((aux_stmts, call_items))
}

fn transform_subscript_items<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    indices: &'ast [ListItem<'src>],
    span: &Span,
) -> TfResult<(PyBlock<'src>, SPyExpr<'src>)> {
    let mut aux_stmts = PyBlock::new();

    let single_item = if indices.len() == 1 {
        match &indices[0] {
            ListItem::Item(item) => Some(item),
            ListItem::Spread(_) => None,
        }
    } else {
        None
    };

    let subscript_expr = if let Some(single_item) = single_item {
        let e = single_item.transform_with_deep_placeholder_guard(ctx)?;
        aux_stmts.extend(e.pre_stmts);
        e.expr
    } else {
        (
            PyExpr::Tuple(
                indices
                    .into_iter()
                    .map(|i| match i {
                        ListItem::Item(expr) => {
                            let e = expr.transform_with_deep_placeholder_guard(ctx)?;
                            aux_stmts.extend(e.pre_stmts);
                            Ok(PyListItem::Item(e.expr))
                        }
                        ListItem::Spread(expr) => {
                            let e = expr.transform_with_deep_placeholder_guard(ctx)?;
                            aux_stmts.extend(e.pre_stmts);
                            Ok(PyListItem::Spread(e.expr))
                        }
                    })
                    .collect::<TfResult<Vec<_>>>()?,
            ),
            *span,
        )
            .into()
    };

    Ok((aux_stmts, subscript_expr))
}

struct PlaceholderCtx {
    activated: bool,
    span: Span,
}

impl PlaceholderCtx {
    fn new(span: Span) -> Self {
        Self {
            activated: false,
            span,
        }
    }

    fn var_name<'src>(&self, ctx: &TfCtx<'src>) -> Cow<'src, str> {
        ctx.temp_var_name("ph", self.span.start).into()
    }
}

fn placeholder_guard<'src, F>(
    ctx: &mut TfCtx<'src>,
    span: &Span,
    f: F,
) -> TfResult<PyExprWithPre<'src>>
where
    F: FnOnce(&mut TfCtx<'src>) -> TfResult<PyExprWithPre<'src>>,
{
    ctx.placeholder_ctx_stack.push(PlaceholderCtx::new(*span));
    let inner_expr = f(ctx)?;
    let popped = ctx.placeholder_ctx_stack.pop().unwrap();

    if popped.activated {
        let var_name = popped.var_name(ctx);

        let mut body = PyBlock::new();
        body.extend(inner_expr.pre_stmts);
        body.push((PyStmt::Return(inner_expr.expr), *span).into());

        let fn_exp = make_fn_exp(
            ctx,
            FnDefArgs::PyArgList(vec![PyArgDefItem::Arg(var_name, None)]),
            FnDefBody::PyStmts(body),
            span,
        )?;

        Ok(PyExprWithPre {
            expr: fn_exp.expr,
            pre_stmts: fn_exp.pre_stmts,
        })
    } else {
        Ok(inner_expr)
    }
}

fn transform_placeholder<'src>(
    ctx: &mut TfCtx<'src>,
    span: &Span,
    access_ctx: PyAccessCtx,
) -> TfResult<PyExprWithPre<'src>> {
    unsafe {
        let raw_ctx = ctx as *mut TfCtx<'src>;
        let ph_ctx = ctx.placeholder_ctx_stack.last_mut().ok_or_else(|| {
            TfErrBuilder::default()
                .message("Placeholder expression outside of placeholder context")
                .span(*span)
                .build_errs()
        })?;

        ph_ctx.activated = true;
        let var_name = ph_ctx.var_name(&*raw_ctx);

        Ok(PyExprWithPre {
            expr: (PyExpr::Ident(var_name, access_ctx), *span).into(),
            pre_stmts: PyBlock::new(),
        })
    }
}

fn transform_postfix_expr<'src, 'ast>(
    ctx: &mut TfCtx<'src>,
    expr: &'ast SExpr<'src>,
    access_ctx: PyAccessCtx,
) -> TfResult<PyExprWithPre<'src>> {
    let mut aux = PyBlock::new();
    let (lift_lhs, lhs_node) = match &expr.0 {
        Expr::Attribute(obj, _) => (false, obj),
        Expr::Subscript(obj, _) => (false, obj),
        Expr::Call(obj, _) => (false, obj),
        Expr::Then(obj, _) => (false, obj),
        Expr::MappedAttribute(obj, _) => (true, obj),
        Expr::MappedSubscript(obj, _) => (true, obj),
        Expr::MappedCall(obj, _) => (true, obj),
        Expr::MappedThen(obj, _) => (true, obj),
        _ => {
            return Err(TfErrBuilder::default()
                .message("Internal error: Postfix expressions can only be attributes, subscripts, calls, or mapped expressions")
                .span(expr.1)
                .build_errs());
        }
    };

    if lift_lhs && access_ctx != PyAccessCtx::Load {
        return Err(TfErrBuilder::default()
            .message("Internal error: Cannot use null-coalescing in a non-Load context")
            .span(expr.1)
            .build_errs());
    }

    let lhs = if lift_lhs {
        let t = lhs_node.transform_lifted(ctx)?;
        aux.extend(t.pre_stmts);
        t.expr
    } else {
        let t = lhs_node.transform(ctx)?;
        aux.extend(t.pre_stmts);
        t.expr
    };

    placeholder_guard(ctx, &expr.1, |ctx| {
        let a = PyAstBuilder::new(expr.1);

        let guard_if_expr = |e| {
            a.if_expr(
                a.call(a.load_ident("__coalesces"), vec![a.call_arg(lhs.clone())]),
                lhs.clone(),
                e,
            )
        };

        let node = match &expr.0 {
            Expr::Call(_, list) => {
                let t = transform_call_items(ctx, &list, &expr.1)?;
                aux.extend(t.0);
                a.call(lhs, t.1)
            }
            Expr::MappedCall(_, list) => {
                let t = transform_call_items(ctx, &list, &expr.1)?;
                aux.extend(t.0);
                guard_if_expr(a.call(lhs.clone(), t.1))
            }
            Expr::Subscript(_, list) => {
                let t = transform_subscript_items(ctx, &list, &expr.1)?;
                aux.extend(t.0);
                a.subscript(lhs, t.1, access_ctx)
            }
            Expr::MappedSubscript(_, list) => {
                let t = transform_subscript_items(ctx, &list, &expr.1)?;
                aux.extend(t.0);
                guard_if_expr(a.subscript(lhs.clone(), t.1, access_ctx))
            }
            Expr::Attribute(_, attr) => a.attribute(lhs, attr.0, access_ctx),
            Expr::MappedAttribute(_, attr) => {
                guard_if_expr(a.attribute(lhs.clone(), attr.0, access_ctx))
            }
            Expr::Then(_, rhs) => {
                let rhs_node = rhs.transform_with_placeholder_guard(ctx)?;
                aux.extend(rhs_node.pre_stmts);
                a.call(rhs_node.expr, vec![PyCallItem::Arg(lhs)])
            }
            Expr::MappedThen(_, rhs) => {
                let rhs_node = rhs.transform_with_placeholder_guard(ctx)?;
                aux.extend(rhs_node.pre_stmts);
                guard_if_expr(a.call(rhs_node.expr, vec![PyCallItem::Arg(lhs.clone())]))
            }
            _ => {
                return Err(TfErrBuilder::default()
                .message("Internal error: Postfix expressions can only be attributes, subscripts, calls, or mapped expressions")
                .span(expr.1)
                .build_errs());
            }
        };

        Ok(PyExprWithPre {
            expr: node,
            pre_stmts: aux,
        })
    })
}

trait ExceptTypesExt<'src> {
    fn transform<'ast>(&'ast self, ctx: &mut TfCtx<'src>, span: &Span) -> TfResult<SPyExpr<'src>>;
}

impl<'src> ExceptTypesExt<'src> for ExceptTypes<'src> {
    fn transform<'ast>(&'ast self, ctx: &mut TfCtx<'src>, span: &Span) -> TfResult<SPyExpr<'src>> {
        match self {
            ExceptTypes::Single(typ) => {
                let typ_node = typ.transform(ctx)?;
                if !typ_node.pre_stmts.is_empty() {
                    return Err(TfErrBuilder::default()
                        .message("Internal error: Type in except clause cannot have pre-statements")
                        .span(typ_node.expr.tl_span)
                        .build_errs());
                }

                Ok(typ_node.expr)
            }
            ExceptTypes::Multiple(types) => {
                let mut type_nodes = vec![];

                for typ in types {
                    let typ_node = typ.transform(ctx)?;

                    if !typ_node.pre_stmts.is_empty() {
                        return Err(TfErrBuilder::default()
                            .message(
                                "Internal error: Type in except clause cannot have pre-statements",
                            )
                            .span(typ_node.expr.tl_span)
                            .build_errs());
                    }

                    type_nodes.push(PyListItem::Item(typ_node.expr));
                }

                Ok((PyExpr::Tuple(type_nodes), *span).into())
            }
        }
    }
}

trait SExprExt<'src> {
    fn transform<'ast>(&'ast self, ctx: &mut TfCtx<'src>) -> TfResult<PyExprWithPre<'src>>;

    fn transform_lifted<'ast>(&'ast self, ctx: &mut TfCtx<'src>) -> TfResult<PyExprWithPre<'src>>;

    /**
     * Transforms the expression, setting a placeholder guard so that
     * expr($, ...) will transform into x => expr(x, ...)
     */
    fn transform_with_placeholder_guard<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyExprWithPre<'src>>;

    /**
     * Transforms the expression, setting a placeholder guard one level deeper
     * so that
     * $ will transform into x => parent_expr(x)
     * and
     * expr will transform into transform_with_placeholder_guard(expr)
     */
    fn transform_with_deep_placeholder_guard<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyExprWithPre<'src>>;

    fn transform_with_access<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
        py_ctx: PyAccessCtx,
    ) -> TfResult<PyExprWithPre<'src>>;
}

impl<'src> SExprExt<'src> for SExpr<'src> {
    fn transform<'ast>(&'ast self, ctx: &mut TfCtx<'src>) -> TfResult<PyExprWithPre<'src>> {
        self.transform_with_access(ctx, PyAccessCtx::Load)
    }

    /**
     * Transforms
     * expr
     * to
     * x = expr
     * x
     */
    fn transform_lifted<'ast>(&'ast self, ctx: &mut TfCtx<'src>) -> TfResult<PyExprWithPre<'src>> {
        let mut aux_stmts = PyBlock::new();
        let value = self.transform(ctx)?;
        aux_stmts.extend(value.pre_stmts);

        let expr = match self.0 {
            Expr::Ident(..) => value.expr,
            _ => {
                let temp_var = ctx.temp_var_name("tmp", self.1.start);

                aux_stmts.push(
                    (
                        PyStmt::Assign(
                            (
                                PyExpr::Ident(temp_var.clone().into(), PyAccessCtx::Store),
                                self.1,
                            )
                                .into(),
                            value.expr,
                        ),
                        self.1,
                    )
                        .into(),
                );

                (PyExpr::Ident(temp_var.into(), PyAccessCtx::Load), self.1).into()
            }
        };

        Ok(PyExprWithPre {
            expr,
            pre_stmts: aux_stmts,
        })
    }

    fn transform_with_placeholder_guard<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyExprWithPre<'src>> {
        placeholder_guard(ctx, &self.1, |ctx| self.transform(ctx))
    }

    fn transform_with_deep_placeholder_guard<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
    ) -> TfResult<PyExprWithPre<'src>> {
        match &self.0 {
            Expr::Placeholder => transform_placeholder(ctx, &self.1, PyAccessCtx::Load),
            _ => self.transform_with_placeholder_guard(ctx),
        }
    }

    fn transform_with_access<'ast>(
        &'ast self,
        ctx: &mut TfCtx<'src>,
        access_ctx: PyAccessCtx,
    ) -> TfResult<PyExprWithPre<'src>> {
        let (expr, span) = self;

        match &expr {
            Expr::Attribute(..) | Expr::Subscript(..) | Expr::Ident(..) => {}
            _ => {
                if access_ctx != PyAccessCtx::Load {
                    return Err(TfErrBuilder::default()
                        .message("Expression context must be Load for this expression")
                        .span(*span)
                        .build_errs());
                }
            }
        }

        match &expr {
            Expr::Checked(expr, exc_types) => {
                let a = PyAstBuilder::new(*span);
                let t = expr.transform(ctx)?;
                let var_name = ctx.temp_var_name("chk", span.start);

                let mut try_body = t.pre_stmts;
                try_body.push(a.assign(a.ident(var_name.clone(), PyAccessCtx::Store), t.expr));

                let mut catch_body = PyBlock::new();
                catch_body.push(a.assign(
                    a.ident(var_name.clone(), PyAccessCtx::Store),
                    a.load_ident("__e"),
                ));

                let mut stmts = PyBlock::new();

                let exc_types = if let Some(exc_types) = exc_types {
                    let t = exc_types.transform(ctx, span)?;
                    Some(t)
                } else {
                    None
                };

                stmts.push(a.try_(
                    try_body,
                    vec![a.except_handler(exc_types, Some("__e"), catch_body)],
                    None,
                ));

                Ok(PyExprWithPre {
                    pre_stmts: stmts,
                    expr: a.load_ident(var_name),
                })
            }
            Expr::Placeholder => transform_placeholder(ctx, span, access_ctx),
            Expr::Fn(arglist, body) => make_fn_exp(
                ctx,
                FnDefArgs::ArgList(arglist),
                FnDefBody::Block(body),
                span,
            ),
            Expr::Class(bases, body) => {
                let name = Cow::<'src, str>::Owned(ctx.temp_var_name("clsexp", span.start));
                let aux_stmts =
                    make_class_def(ctx, name.clone(), bases, body, PyDecorators::new(), span)?;

                Ok(PyExprWithPre {
                    expr: (PyExpr::Ident(name, PyAccessCtx::Load), *span).into(),
                    pre_stmts: aux_stmts,
                })
            }
            Expr::Literal((lit, span)) => {
                let value = match lit {
                    Literal::Num(num) => {
                        (PyExpr::Literal(PyLiteral::Num(num.to_owned())), *span).into()
                    }
                    Literal::Str(s) => {
                        (PyExpr::Literal(PyLiteral::Str(s.to_owned())), *span).into()
                    }
                    Literal::Bool(b) => (PyExpr::Literal(PyLiteral::Bool(*b)), *span).into(),
                    Literal::None => (PyExpr::Literal(PyLiteral::None), *span).into(),
                };

                Ok(PyExprWithPre {
                    expr: value,
                    pre_stmts: PyBlock::new(),
                })
            }
            Expr::Ident((ident, span)) => Ok(PyExprWithPre {
                expr: (PyExpr::Ident(Cow::Borrowed(ident), access_ctx), *span).into(),
                pre_stmts: PyBlock::new(),
            }),
            Expr::Attribute(..)
            | Expr::MappedAttribute(..)
            | Expr::Call(..)
            | Expr::MappedCall(..)
            | Expr::Subscript(..)
            | Expr::MappedSubscript(..)
            | Expr::Then(..)
            | Expr::MappedThen(..) => transform_postfix_expr(ctx, self, access_ctx),
            Expr::If(cond, then_block, else_block) => {
                transform_if_expr(ctx, cond, then_block, else_block, span)
            }
            Expr::Block(block) => {
                let PyBlockWithFinal { stmts, final_ } = block.transform_with_final_expr(ctx)?;

                let expr = if let BlockFinal::Expr(final_) = final_ {
                    final_
                } else {
                    return Err(TfErrBuilder::default()
                        .message("block-expression must have a final expression")
                        .span(block.1)
                        .build_errs());
                };

                Ok(PyExprWithPre {
                    expr,
                    pre_stmts: stmts,
                })
            }
            Expr::Match(subject, cases) => transform_match_expr(ctx, subject, cases, span),
            Expr::Binary(op, lhs, rhs) => {
                let (lhs, rhs) = match op {
                    BinaryOp::Pipe => {
                        let lhs = lhs.transform_with_placeholder_guard(ctx)?;
                        let rhs = rhs.transform_with_placeholder_guard(ctx)?;

                        (lhs, rhs)
                    }
                    BinaryOp::Coalesce => {
                        let lhs = lhs.transform_lifted(ctx)?;
                        let rhs = rhs.transform(ctx)?;

                        (lhs, rhs)
                    }
                    _ => (lhs.transform(ctx)?, rhs.transform(ctx)?),
                };

                let mut aux_stmts = lhs.pre_stmts;
                aux_stmts.extend(rhs.pre_stmts);

                let py_op = match op {
                    BinaryOp::Add => PyBinaryOp::Add,
                    BinaryOp::Sub => PyBinaryOp::Sub,
                    BinaryOp::Mul => PyBinaryOp::Mult,
                    BinaryOp::Div => PyBinaryOp::Div,
                    BinaryOp::Mod => PyBinaryOp::Mod,
                    BinaryOp::Exp => PyBinaryOp::Pow,
                    BinaryOp::MatMul => PyBinaryOp::MatMult,

                    BinaryOp::Lt => PyBinaryOp::Lt,
                    BinaryOp::Gt => PyBinaryOp::Gt,
                    BinaryOp::Leq => PyBinaryOp::Leq,
                    BinaryOp::Geq => PyBinaryOp::Geq,
                    BinaryOp::Eq => PyBinaryOp::Eq,
                    BinaryOp::Neq => PyBinaryOp::Neq,
                    BinaryOp::Is => PyBinaryOp::Is,
                    BinaryOp::Nis => PyBinaryOp::Nis,

                    BinaryOp::Pipe => {
                        return Ok(PyExprWithPre {
                            expr: (
                                PyExpr::Call(Box::new(rhs.expr), vec![PyCallItem::Arg(lhs.expr)]),
                                *span,
                            )
                                .into(),
                            pre_stmts: aux_stmts,
                        });
                    }

                    BinaryOp::Coalesce => {
                        let a = PyAstBuilder::new(*span);

                        let expr = a.if_expr(
                            a.call(
                                a.load_ident("__coalesces"),
                                vec![a.call_arg(lhs.expr.clone())],
                            ),
                            rhs.expr,
                            lhs.expr,
                        );

                        return Ok(PyExprWithPre {
                            expr,
                            pre_stmts: aux_stmts,
                        });
                    }
                };

                return Ok(PyExprWithPre {
                    expr: (
                        PyExpr::Binary(py_op, Box::new(lhs.expr), Box::new(rhs.expr)),
                        *span,
                    )
                        .into(),
                    pre_stmts: aux_stmts,
                });
            }
            Expr::Unary(op, expr) => {
                let expr = expr.transform(ctx)?;
                let aux_stmts = expr.pre_stmts;

                let py_op = match op {
                    UnaryOp::Neg => PyUnaryOp::Neg,
                    UnaryOp::Pos => PyUnaryOp::Pos,
                    UnaryOp::Inv => PyUnaryOp::Inv,
                    UnaryOp::Yield => {
                        return Ok(PyExprWithPre {
                            expr: (PyExpr::Yield(Box::new(expr.expr)), *span).into(),
                            pre_stmts: aux_stmts,
                        });
                    }
                    UnaryOp::YieldFrom => {
                        return Ok(PyExprWithPre {
                            expr: (PyExpr::YieldFrom(Box::new(expr.expr)), *span).into(),
                            pre_stmts: aux_stmts,
                        });
                    }
                };

                return Ok(PyExprWithPre {
                    expr: (PyExpr::Unary(py_op, Box::new(expr.expr)), *span).into(),
                    pre_stmts: aux_stmts,
                });
            }
            Expr::List(exprs) => {
                return placeholder_guard(ctx, span, |ctx| {
                    let mut aux_stmts = PyBlock::new();
                    let mut items = vec![];

                    for expr in exprs {
                        let e = match expr {
                            ListItem::Spread(expr) => {
                                expr.transform_with_deep_placeholder_guard(ctx)?
                            }
                            ListItem::Item(expr) => {
                                expr.transform_with_deep_placeholder_guard(ctx)?
                            }
                        };
                        aux_stmts.extend(e.pre_stmts);
                        items.push(match expr {
                            ListItem::Spread(_) => PyListItem::Spread(e.expr),
                            ListItem::Item(_) => PyListItem::Item(e.expr),
                        });
                    }

                    return Ok(PyExprWithPre {
                        expr: (PyExpr::List(items), *span).into(),
                        pre_stmts: aux_stmts,
                    });
                });
            }
            Expr::Mapping(items) => {
                return placeholder_guard(ctx, span, |ctx| {
                    let mut aux_stmts = PyBlock::new();
                    let mut dict_items = vec![];

                    for item in items {
                        match item {
                            MappingItem::Item(key, value) => {
                                let key = key.transform_with_deep_placeholder_guard(ctx)?;
                                let value = value.transform_with_deep_placeholder_guard(ctx)?;

                                aux_stmts.extend(key.pre_stmts);
                                aux_stmts.extend(value.pre_stmts);

                                dict_items.push(PyDictItem::Item(key.expr, value.expr));
                            }
                            MappingItem::Spread(expr) => {
                                let e = expr.transform_with_deep_placeholder_guard(ctx)?;
                                aux_stmts.extend(e.pre_stmts);

                                dict_items.push(PyDictItem::Spread(e.expr));
                            }
                        }
                    }

                    return Ok(PyExprWithPre {
                        expr: (PyExpr::Dict(dict_items), *span).into(),
                        pre_stmts: aux_stmts,
                    });
                });
            }
            Expr::Slice(start, end, step) => {
                return placeholder_guard(ctx, span, |ctx| {
                    let start_node = start
                        .as_ref()
                        .map(|e| e.as_ref().transform_with_deep_placeholder_guard(ctx))
                        .transpose()?;
                    let end_node = end
                        .as_ref()
                        .map(|e| e.as_ref().transform_with_deep_placeholder_guard(ctx))
                        .transpose()?;
                    let step_node = step
                        .as_ref()
                        .map(|e| e.as_ref().transform_with_deep_placeholder_guard(ctx))
                        .transpose()?;

                    let mut aux_stmts = PyBlock::new();

                    let mut get = |x: Option<PyExprWithPre<'src>>| {
                        let expr = if let Some(x) = x {
                            aux_stmts.extend(x.pre_stmts);
                            x.expr
                        } else {
                            (PyExpr::Literal(PyLiteral::None), *span).into()
                        };

                        PyCallItem::Arg(expr)
                    };

                    return Ok(PyExprWithPre {
                        expr: (
                            PyExpr::Call(
                                Box::new(
                                    (PyExpr::Ident("slice".into(), PyAccessCtx::Load), *span)
                                        .into(),
                                ),
                                vec![get(start_node), get(end_node), get(step_node)],
                            ),
                            *span,
                        )
                            .into(),
                        pre_stmts: aux_stmts,
                    });
                });
            }
            Expr::Fstr(begin, parts) => {
                return placeholder_guard(ctx, span, |ctx| {
                    let mut aux_stmts = PyBlock::new();
                    let mut nodes = Vec::new();

                    nodes.push(PyFstrPart::Str(begin.0.clone().into()));

                    for (fmt_expr, str_part) in parts {
                        // TODO format specifiers?
                        let block_node = fmt_expr.0.block.transform_with_final_expr(ctx)?;
                        aux_stmts.extend(block_node.stmts);

                        let expr_node = if let BlockFinal::Expr(final_) = block_node.final_ {
                            final_
                        } else {
                            return Err(TfErrBuilder::default()
                                .message("f-string expression must have a final expression")
                                .span(fmt_expr.1)
                                .build_errs());
                        };

                        nodes.push(PyFstrPart::Expr(expr_node, "".into()));
                        nodes.push(PyFstrPart::Str(str_part.0.clone().into()));
                    }

                    let expr = (PyExpr::Fstr(nodes), *span).into();
                    return Ok(PyExprWithPre {
                        expr,
                        pre_stmts: aux_stmts,
                    });
                });
            }
        }
    }
}

pub struct TransformOutput<'src> {
    pub py_block: PyBlock<'src>,
    pub exports: Vec<PyIdent<'src>>,
    pub module_star_exports: Vec<PyIdent<'src>>,
}

pub fn transform_ast<'src, 'ast>(
    source: &'src str,
    block: &'ast SBlock<'src>,
    treat_final_as_expr: bool,
) -> TfResult<TransformOutput<'src>> {
    let mut ctx = TfCtx::new(source)?;
    let mut stmts = block.transform(&mut ctx, treat_final_as_expr, true)?;

    if let BlockFinal::Expr(final_expr) = stmts.final_ {
        if treat_final_as_expr {
            let span = final_expr.tl_span;
            stmts.stmts.push((PyStmt::Expr(final_expr), span).into());
        } else {
            panic!("there shouldn't be a final expr");
        }
    }

    Ok(TransformOutput {
        py_block: stmts.stmts,
        exports: ctx.exports,
        module_star_exports: ctx.module_star_exports,
    })
}
