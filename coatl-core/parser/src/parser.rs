#![allow(dead_code)]

use std::borrow::Cow;

use crate::ast::*;
use crate::lexer::*;
use chumsky::{extra::ParserExtra, input::ValueInput, prelude::*};

fn enumeration<'tokens, 'src: 'tokens, I, O: 'tokens, OS: 'tokens, E, ItemParser, SepParser>(
    item_parser: ItemParser,
    optional_separator: SepParser,
) -> impl Parser<'tokens, I, Vec<O>, E> + Clone
where
    I: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
    E: ParserExtra<'tokens, I, Error = Rich<'tokens, Token<'src>, Span>>,
    ItemParser: Parser<'tokens, I, O, E> + Clone + 'tokens,
    SepParser: Parser<'tokens, I, OS, E> + Clone + 'tokens,
{
    choice((
        item_parser
            .clone()
            .separated_by(choice((
                optional_separator
                    .clone()
                    .then(just(Token::Eol).or_not())
                    .ignored(),
                optional_separator
                    .clone()
                    .or_not()
                    .then(just(Token::Eol))
                    .ignored(),
            )))
            .allow_trailing()
            .collect()
            .delimited_by(
                just(Token::Symbol("BEGIN_BLOCK")),
                just(Token::Symbol("END_BLOCK")),
            )
            .labelled("block enumeration"),
        item_parser
            .separated_by(optional_separator)
            .allow_trailing()
            .collect()
            .labelled("inline enumeration"),
    ))
    .labelled("enumeration")
    .boxed()
}

pub trait ParserExt<'tokens, 'src: 'tokens, I, O, E>: Parser<'tokens, I, O, E>
where
    I: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
    E: ParserExtra<'tokens, I>,
{
    fn spanned(self) -> impl Parser<'tokens, I, Spanned<O>, E> + Clone + Sized
    where
        Self: Sized + Clone,
    {
        self.map_with(|x, e| (x, e.span()))
    }

    fn delimited_by_with_eol(
        self,
        start: impl Parser<'tokens, I, Token<'src>, E> + Clone,
        end: impl Parser<'tokens, I, Token<'src>, E> + Clone,
    ) -> impl Parser<'tokens, I, O, E> + Clone
    where
        Self: Sized + Clone,
    {
        self.delimited_by(start, just(Token::Eol).or_not().then(end))
    }
}

impl<'tokens, 'src: 'tokens, I, O, E, P> ParserExt<'tokens, 'src, I, O, E> for P
where
    P: Parser<'tokens, I, O, E> + Sized,
    I: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
    E: ParserExtra<'tokens, I>,
{
}

const START_BLOCK: Token = Token::Symbol(":");
type TExtra<'tokens, 'src> = extra::Err<Rich<'tokens, Token<'src>, Span>>;

pub fn symbol<'tokens, 'src: 'tokens, I>(
    symbol: &'static str,
) -> impl Parser<'tokens, I, Token<'src>, TExtra<'tokens, 'src>> + Clone
where
    I: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
{
    just(Token::Symbol(symbol))
}

pub fn match_pattern<'tokens, 'src: 'tokens, TInput, PIdent, PQualIdent, PLiteral>(
    ident: PIdent,
    qualified_ident: PQualIdent,
    literal: PLiteral,
) -> impl Parser<'tokens, TInput, SPattern<'src>, TExtra<'tokens, 'src>> + Clone
where
    TInput: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
    PIdent: Parser<'tokens, TInput, SIdent<'src>, TExtra<'tokens, 'src>> + Clone + 'tokens,
    PQualIdent: Parser<'tokens, TInput, SExpr<'src>, TExtra<'tokens, 'src>> + Clone + 'tokens,
    PLiteral: Parser<'tokens, TInput, SExpr<'src>, TExtra<'tokens, 'src>> + Clone + 'tokens,
{
    let mut pattern = Recursive::declare();

    let literal_pattern = literal.map(Pattern::Value).spanned().boxed();

    let capture = ident
        .clone()
        .map(|x| if x.0 == "_" { None } else { Some(x) })
        .boxed();

    let capture_pattern = capture.clone().map(Pattern::Capture).spanned().boxed();

    let value_pattern = symbol(".")
        .or_not()
        .ignore_then(qualified_ident.clone())
        .map(Pattern::Value)
        .spanned()
        .boxed();

    let group_pattern = pattern
        .clone()
        .delimited_by_with_eol(symbol("("), symbol(")"))
        .boxed();

    let sequence_item = choice((
        symbol("*")
            .ignore_then(capture.clone())
            .map(PatternSequenceItem::Spread),
        pattern.clone().map(PatternSequenceItem::Item),
    ))
    .boxed();

    let sequence_pattern = sequence_item
        .separated_by(symbol(",").then(just(Token::Eol).or_not()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(Pattern::Sequence)
        .delimited_by_with_eol(symbol("["), symbol("]"))
        .spanned()
        .boxed();

    let mapping_item = choice((
        symbol("**")
            .ignore_then(capture.clone())
            .map(PatternMappingItem::Spread),
        ident
            .clone()
            .then_ignore(symbol(":"))
            .then(pattern.clone())
            .map(|(key, value)| PatternMappingItem::Item(key, value)),
    ));

    let mapping_pattern = mapping_item
        .separated_by(symbol(",").then(just(Token::Eol).or_not()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(Pattern::Mapping)
        .delimited_by_with_eol(symbol("["), symbol("]"))
        .spanned()
        .boxed();

    let class_item = choice((
        ident
            .clone()
            .then_ignore(symbol("="))
            .then(pattern.clone())
            .map(|(key, value)| PatternClassItem::Kw(key, value)),
        pattern.clone().map(PatternClassItem::Item),
    ))
    .boxed();

    let class_pattern = qualified_ident
        .clone()
        .then(
            class_item
                .separated_by(symbol(","))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by_with_eol(symbol("("), symbol(")")),
        )
        .map(|(a, b)| Pattern::Class(a, b))
        .spanned()
        .boxed();

    let closed_pattern = choice((
        literal_pattern,
        capture_pattern,
        group_pattern,
        sequence_pattern,
        mapping_pattern,
        class_pattern,
    ))
    .boxed();

    let or_pattern = closed_pattern
        .clone()
        .separated_by(symbol("|").then(just(Token::Eol).or_not()))
        .allow_leading()
        .collect::<Vec<_>>()
        .map_with(|x, e| {
            if x.len() == 1 {
                x.into_iter().next().unwrap()
            } else {
                (Pattern::Or(x), e.span())
            }
        });

    let as_pattern = or_pattern
        .then(symbol("as").ignore_then(ident.clone()).or_not())
        .map_with(|(pattern, as_ident), e| {
            if let Some(as_ident) = as_ident {
                (Pattern::As(Box::new(pattern), as_ident), e.span())
            } else {
                pattern
            }
        })
        .boxed();

    pattern.define(
        choice((
            as_pattern,
            value_pattern,
            closed_pattern,
            symbol("_").to(Pattern::Capture(None)).spanned(),
        ))
        .labelled("pattern"),
    );

    pattern.labelled("pattern").as_context()
}

pub fn function<'tokens, 'src: 'tokens, TInput, PL, PR, PIdent, PExpr>(
    monic_lhs: PL,
    block_or_inline_stmt: PR,
    ident: PIdent,
    expr: PExpr,
) -> impl Parser<'tokens, TInput, SExpr<'src>, TExtra<'tokens, 'src>> + Clone
where
    TInput: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
    PL: Parser<'tokens, TInput, SExpr<'src>, TExtra<'tokens, 'src>> + Clone + 'tokens,
    PR: Parser<'tokens, TInput, SBlock<'src>, TExtra<'tokens, 'src>> + Clone + 'tokens,
    PIdent: Parser<'tokens, TInput, SIdent<'src>, TExtra<'tokens, 'src>> + Clone + 'tokens,
    PExpr: Parser<'tokens, TInput, SExpr<'src>, TExtra<'tokens, 'src>> + Clone + 'tokens,
{
    recursive(|fn_| {
        let fn_body = symbol("=>")
            .ignore_then(choice((
                block_or_inline_stmt.clone(),
                fn_.clone().map(|x| Block::Expr(x)).spanned().boxed(),
            )))
            .boxed();

        let monic_fn = monic_lhs
            .clone()
            .then(fn_body.clone().or_not())
            .map_with(|(x, body), e| {
                if let Some(body) = body {
                    (
                        Expr::Fn(vec![ArgDefItem::Arg(x, None)], Box::new(body)),
                        e.span(),
                    )
                } else {
                    x
                }
            })
            .labelled("unary-fn")
            .boxed();

        let arg_list = enumeration(
            choice((
                symbol("*")
                    .ignore_then(ident.clone())
                    .map(|x| ArgDefItem::ArgSpread(x)),
                symbol("**")
                    .ignore_then(ident.clone())
                    .map(ArgDefItem::KwargSpread),
                expr.clone()
                    .then(symbol("=").ignore_then(expr.clone()).or_not())
                    .map(|(key, value)| ArgDefItem::Arg(key, value)),
            )),
            symbol(","),
        )
        .labelled("argument-def-list")
        .boxed();

        let nary_fn = arg_list
            .clone()
            .delimited_by_with_eol(symbol("("), symbol(")"))
            .then(fn_body)
            .map(|(args, body)| Expr::Fn(args, Box::new(body)))
            .spanned()
            .labelled("nary-fn")
            .as_context()
            .boxed();

        choice((nary_fn, monic_fn)).labelled("fn")
    })
}

pub fn parser<'tokens, 'src: 'tokens, TInput>()
-> impl Parser<'tokens, TInput, SBlock<'src>, extra::Err<Rich<'tokens, Token<'src>, Span>>>
where
    TInput: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
{
    let mut stmt = Recursive::declare();
    let mut inline_stmt = Recursive::declare();
    let mut atom = Recursive::declare();
    let mut sexpr = Recursive::declare();

    let stmts = stmt
        .clone()
        .repeated()
        .collect::<Vec<_>>()
        .map(Block::Stmts)
        .spanned()
        .labelled("statement-list")
        .boxed();

    let block = stmts
        .clone()
        .delimited_by(symbol("BEGIN_BLOCK"), symbol("END_BLOCK"))
        .boxed();

    let block_or_expr = choice((block.clone(), sexpr.clone().map(Block::Expr).spanned())).boxed();

    let block_or_inline_stmt = choice((
        block.clone(),
        inline_stmt
            .clone()
            .map(|x| Block::Stmts(vec![x]))
            .spanned()
            .boxed(),
    ))
    .boxed();

    let literal = select! {
        Token::Num(s) => Literal::Num(Cow::Borrowed(s)),
        Token::Str(s) => Literal::Str(Cow::Owned(s)),
        Token::Bool(s) => Literal::Bool(s),
        Token::None => Literal::None
    }
    .spanned()
    .boxed();

    let literal_expr = literal
        .clone()
        .map(Expr::Literal)
        .labelled("literal")
        .spanned()
        .boxed();

    let ident = select! {
        Token::Ident(s) => s,
    }
    .spanned()
    .labelled("identifier")
    .boxed();

    let ident_expr = ident
        .clone()
        .map(Expr::Ident)
        .spanned()
        .labelled("identifier-expression")
        .boxed();

    let placeholder = select! {
        Token::Symbol("$") => Expr::Placeholder,
    }
    .spanned()
    .labelled("placeholder")
    .boxed();

    let list_item = choice((
        symbol("*").ignore_then(sexpr.clone()).map(ListItem::Spread),
        sexpr.clone().map(ListItem::Item),
    ))
    .boxed();

    let list = enumeration(list_item.clone(), symbol(","))
        .delimited_by_with_eol(symbol("["), symbol("]"))
        .map(Expr::List)
        .labelled("list")
        .as_context()
        .spanned()
        .boxed();

    let nary_tuple = group((
        list_item.clone(),
        symbol(",")
            .ignore_then(list_item)
            .repeated()
            .collect::<Vec<_>>(),
        symbol(",").to(1).or_not(),
    ))
    .try_map_with(
        |(first, rest, last_comma), e| -> Result<SExpr, Rich<'tokens, Token<'src>, Span>> {
            let mut items = Vec::<ListItem>::new();

            match first {
                ListItem::Item(expr) if rest.is_empty() && last_comma.is_none() => {
                    return Ok(expr);
                }
                // ListItem::Spread(expr) if rest.is_empty() && last_comma.is_none() => {
                //     // should this be an error?

                //     return Err(Rich::custom(
                //         first.1,
                //         "Spread operator must be in a list or tuple",
                //     ));
                // }
                ListItem::Item(..) => {
                    items.push(first);
                }
                ListItem::Spread(..) => {
                    items.push(first);
                }
            }
            items.extend(rest);

            Ok((Expr::Tuple(items), e.span()))
        },
    )
    .labelled("nary-tuple")
    .boxed();

    let mapping = enumeration(
        choice((
            symbol("**")
                .ignore_then(sexpr.clone())
                .map(MappingItem::Spread),
            sexpr
                .clone()
                .then_ignore(just(START_BLOCK))
                .then(sexpr.clone())
                .map(|(key, value)| MappingItem::Item(key, value)),
        )),
        symbol(","),
    )
    .delimited_by_with_eol(just(Token::Symbol("[")), just(Token::Symbol("]")))
    .map(Expr::Mapping)
    .labelled("mapping")
    .as_context()
    .spanned()
    .boxed();

    let fstr_begin = select! {
        Token::FstrBegin(s) => s,
    };
    let fstr_continue = select! {
        Token::FstrContinue(s) => s,
    };

    let fstr = fstr_begin
        .spanned()
        .then(
            block_or_expr
                .clone()
                .spanned()
                .then(fstr_continue.spanned())
                .map(|(block, cont)| {
                    (
                        (
                            FmtExpr {
                                block: block.0,
                                fmt: None,
                            },
                            block.1,
                        ),
                        cont,
                    )
                })
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map(|(begin, parts)| Expr::Fstr(begin, parts))
        .spanned()
        .labelled("f-string")
        .boxed();

    let class_ = just(Token::Kw("class"))
        .ignore_then(
            enumeration(
                choice((
                    ident
                        .clone()
                        .then_ignore(symbol("="))
                        .then(sexpr.clone())
                        .map(|(key, value)| CallItem::Kwarg(key, value)),
                    sexpr.clone().map(CallItem::Arg),
                ))
                .spanned()
                .boxed(),
                symbol(","),
            )
            .delimited_by_with_eol(just(Token::Symbol("(")), just(Token::Symbol(")")))
            .or_not(),
        )
        .then_ignore(just(START_BLOCK))
        .then(block_or_inline_stmt.clone())
        .map(|(arglist, block)| Expr::Class(arglist.unwrap_or_else(|| Vec::new()), Box::new(block)))
        .spanned()
        .labelled("class")
        .boxed();

    let classic_if = just(Token::Kw("if"))
        .ignore_then(group((
            sexpr.clone().then_ignore(just(START_BLOCK)),
            block_or_inline_stmt.clone(),
            group((
                just(Token::Eol).or_not(),
                just(Token::Kw("else")),
                just(START_BLOCK),
            ))
            .ignore_then(block_or_inline_stmt.clone())
            .or_not(),
        )))
        .map(|(cond, if_, else_)| Expr::If(Box::new(cond), Box::new(if_), else_.map(Box::new)))
        .spanned()
        .labelled("if")
        .boxed();

    let block_expr = just(Token::Kw("block"))
        .then(just(START_BLOCK))
        .ignore_then(block_or_inline_stmt.clone())
        .map(|x| Expr::Block(Box::new(x)))
        .spanned()
        .labelled("block");

    atom.define(
        choice((
            ident_expr.clone(),
            classic_if,
            class_,
            block_expr,
            literal_expr.clone(),
            placeholder,
            list.clone(),
            mapping,
            fstr,
            symbol("(")
                .then(symbol(")"))
                .map(|_| Expr::Tuple(vec![]))
                .spanned(),
            nary_tuple
                .clone()
                .delimited_by_with_eol(just(Token::Symbol("(")), just(Token::Symbol(")"))),
        ))
        .labelled("atom"),
    );

    let qualified_ident = ident_expr
        .clone()
        .foldl_with(
            symbol(".").ignore_then(ident.clone()).repeated(),
            |lhs, rhs, e| (Expr::Attribute(Box::new(lhs), rhs), e.span()),
        )
        .boxed();

    enum Postfix<'a> {
        Call(Vec<SCallItem<'a>>),
        Subscript(Vec<ListItem<'a>>),
        Extension(SExpr<'a>),
        Then(SExpr<'a>),
        Attribute(SIdent<'a>),
    }

    let call_args = enumeration(
        choice((
            symbol("*")
                .ignore_then(sexpr.clone())
                .map(CallItem::ArgSpread),
            symbol("**")
                .ignore_then(sexpr.clone())
                .map(CallItem::KwargSpread),
            ident
                .clone()
                .then_ignore(symbol("="))
                .then(sexpr.clone())
                .map(|(key, value)| CallItem::Kwarg(key, value)),
            sexpr.clone().map(CallItem::Arg),
        ))
        .spanned()
        .boxed(),
        symbol(","),
    )
    .delimited_by_with_eol(just(Token::Symbol("(")), just(Token::Symbol(")")));

    let call = call_args
        .clone()
        .map(Postfix::Call)
        .labelled("argument-list")
        .as_context()
        .boxed();

    let subscript = enumeration(
        choice((
            symbol("*").ignore_then(sexpr.clone()).map(ListItem::Spread),
            sexpr.clone().map(ListItem::Item),
        ))
        .boxed(),
        symbol(","),
    )
    .delimited_by_with_eol(just(Token::Symbol("[")), just(Token::Symbol("]")))
    .map(Postfix::Subscript)
    .labelled("subscript")
    .as_context()
    .boxed();

    let attribute = symbol(".")
        .ignore_then(ident.clone())
        .map(Postfix::Attribute)
        .labelled("attr");

    let then = symbol(".")
        .ignore_then(
            sexpr
                .clone()
                .delimited_by_with_eol(symbol("("), symbol(")"))
                .map(Postfix::Then),
        )
        .labelled("attr")
        .boxed();

    let extension = symbol("!")
        .ignore_then(ident_expr.clone())
        .map(Postfix::Extension)
        .labelled("extension")
        .boxed();

    let expr_extension = symbol("!")
        .ignore_then(
            sexpr
                .clone()
                .delimited_by_with_eol(symbol("("), symbol(")")),
        )
        .map(Postfix::Extension)
        .labelled("extension")
        .boxed();

    let postfix = atom
        .clone()
        .foldl_with(
            symbol("?")
                .to(1)
                .or_not()
                .then(choice((
                    call,
                    subscript,
                    attribute,
                    then,
                    extension,
                    expr_extension,
                )))
                .repeated(),
            |expr, (coal, op), e| -> SExpr {
                (
                    if coal.is_none() {
                        match op {
                            Postfix::Call(args) => Expr::Call(Box::new(expr), args),
                            Postfix::Subscript(args) => Expr::Subscript(Box::new(expr), args),
                            Postfix::Attribute(attr) => Expr::Attribute(Box::new(expr), attr),
                            Postfix::Then(rhs) => Expr::Then(Box::new(expr), Box::new(rhs)),
                            Postfix::Extension(rhs) => {
                                Expr::Extension(Box::new(expr), Box::new(rhs))
                            }
                        }
                    } else {
                        match op {
                            Postfix::Call(args) => Expr::MappedCall(Box::new(expr), args),
                            Postfix::Subscript(args) => Expr::MappedSubscript(Box::new(expr), args),
                            Postfix::Attribute(attr) => Expr::MappedAttribute(Box::new(expr), attr),
                            Postfix::Then(rhs) => Expr::MappedThen(Box::new(expr), Box::new(rhs)),
                            Postfix::Extension(rhs) => {
                                Expr::MappedExtension(Box::new(expr), Box::new(rhs))
                            }
                        }
                    },
                    e.span(),
                )
            },
        )
        .labelled("postfix")
        .boxed();

    let unary = select! {
        Token::Symbol("@") => UnaryOp::Yield,
        Token::Symbol("@@") => UnaryOp::YieldFrom,
        Token::Symbol("+") => UnaryOp::Pos,
        Token::Symbol("-") => UnaryOp::Neg,
        Token::Symbol("~") => UnaryOp::Inv,
    }
    .repeated()
    .foldr_with(postfix, |op: UnaryOp, rhs: SExpr, e| {
        (Expr::Unary(op, Box::new(rhs)), e.span())
    })
    .labelled("unary")
    .boxed();

    fn make_binary_op<'tokens, 'src: 'tokens, I, POp, PArg>(
        arg: PArg,
        op: POp,
        right_assoc: bool,
    ) -> impl Parser<'tokens, I, SExpr<'src>, extra::Err<Rich<'tokens, Token<'src>, Span>>> + Clone
    where
        PArg: Parser<'tokens, I, SExpr<'src>, extra::Err<Rich<'tokens, Token<'src>, Span>>>
            + Clone
            + 'tokens,
        POp: Parser<'tokens, I, BinaryOp, extra::Err<Rich<'tokens, Token<'src>, Span>>>
            + Clone
            + 'tokens,
        I: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
    {
        if !right_assoc {
            arg.clone()
                .foldl_with(op.then(arg).repeated(), |lhs, (op, rhs), e| {
                    (Expr::Binary(op, Box::new(lhs), Box::new(rhs)), e.span())
                })
                .boxed()
        } else {
            recursive(|bin| {
                arg.clone()
                    .then(op.then(bin.or(arg.clone())).or_not())
                    .map_with(|(lhs, matched), e| {
                        if let Some((op, rhs)) = matched {
                            (Expr::Binary(op, Box::new(lhs), Box::new(rhs)), e.span())
                        } else {
                            lhs
                        }
                    })
            })
            .boxed()
        }
    }

    let fn_ = function(
        unary.clone(),
        block_or_inline_stmt.clone(),
        ident.clone(),
        sexpr.clone(),
    );

    let binary0 = make_binary_op(
        fn_.clone(),
        select! {
            Token::Symbol("**") => BinaryOp::Exp,
        },
        true,
    );

    let binary1 = make_binary_op(
        binary0,
        select! {
            Token::Symbol("*") => BinaryOp::Mul,
            Token::Symbol("/") => BinaryOp::Div,
            Token::Symbol("%") => BinaryOp::Mod,
            Token::Symbol("@") => BinaryOp::MatMul,
        },
        false,
    );

    let binary2 = make_binary_op(
        binary1,
        select! {
            Token::Symbol("+") => BinaryOp::Add,
            Token::Symbol("-") => BinaryOp::Sub,
        },
        false,
    );

    let binary3 = make_binary_op(
        binary2.clone(),
        select! {
            Token::Symbol("<") => BinaryOp::Lt,
            Token::Symbol("<=") => BinaryOp::Leq,
            Token::Symbol(">") => BinaryOp::Gt,
            Token::Symbol(">=") => BinaryOp::Geq,
            Token::Symbol("==") => BinaryOp::Eq,
            Token::Symbol("<>") => BinaryOp::Neq,
            Token::Symbol("===") => BinaryOp::Is,
            Token::Symbol("<=>") => BinaryOp::Nis,
        },
        false,
    );

    let except_types = choice((
        qualified_ident.clone().map(ExceptTypes::Single),
        enumeration(qualified_ident.clone(), symbol(","))
            .delimited_by(symbol("["), symbol("]"))
            .map(ExceptTypes::Multiple),
    ))
    .boxed();

    let checked_ = just(Token::Kw("try"))
        .ignore_then(binary2.clone())
        .then(
            just(Token::Kw("except"))
                .ignore_then(except_types.clone())
                .or_not(),
        )
        .map(|(expr, typs)| Expr::Checked(Box::new(expr), typs.map(Box::new)))
        .spanned()
        .labelled("checked")
        .boxed();

    let binary4 = make_binary_op(
        binary3.or(checked_),
        select! {
            Token::Symbol("??") => BinaryOp::Coalesce,
        },
        false,
    );

    let slice0 = group((
        binary4.clone(),
        symbol("..").ignore_then(binary4.clone().or_not()).or_not(),
        symbol("..").ignore_then(binary4.clone().or_not()).or_not(),
    ))
    .map_with(|(lhs, a, b), e| {
        if a.is_none() && b.is_none() {
            lhs
        } else {
            (
                Expr::Slice(
                    Some(Box::new(lhs)),
                    a.flatten().map(Box::new),
                    b.flatten().map(Box::new),
                ),
                e.span(),
            )
        }
    })
    .labelled("slice")
    .boxed();

    let slice1 = symbol("..")
        .ignore_then(binary4.clone().or_not())
        .then(symbol("..").ignore_then(binary4.clone().or_not()).or_not())
        .map(|(e1, e2)| Expr::Slice(None, e1.map(Box::new), e2.flatten().map(Box::new)))
        .spanned()
        .labelled("slice")
        .boxed();

    let slices = choice((slice0, slice1));

    let case_ = choice((
        group((
            match_pattern(ident.clone(), qualified_ident, literal_expr).or_not(),
            just(Token::Kw("if")).ignore_then(sexpr.clone()).or_not(),
            just(START_BLOCK).ignore_then(block_or_inline_stmt.clone()),
        ))
        .map(|(pattern, guard, body)| MatchCase {
            pattern,
            guard,
            body,
        }),
        just(START_BLOCK)
            .or_not()
            .ignore_then(block_or_inline_stmt.clone())
            .map(|x| MatchCase {
                pattern: None,
                guard: None,
                body: x,
            }),
    ))
    .labelled("match-case")
    .boxed();

    let match_ = slices
        .then(
            just(Token::Kw("match"))
                .then(just(START_BLOCK).or_not())
                .ignore_then(enumeration(case_, just(Token::Kw("else"))))
                .or_not(),
        )
        .map_with(|(scrutinee, cases), e| {
            if let Some(cases) = cases {
                (Expr::Match(Box::new(scrutinee), cases), e.span())
            } else {
                scrutinee
            }
        })
        .labelled("match")
        .boxed();

    let if_ = match_
        .then(
            group((
                just(Token::Kw("then"))
                    .then(just(START_BLOCK).or_not())
                    .ignore_then(block_or_inline_stmt.clone()),
                just(Token::Eol)
                    .or_not()
                    .then(just(Token::Kw("else")))
                    .then(just(START_BLOCK).or_not())
                    .ignore_then(block_or_inline_stmt.clone())
                    .or_not(),
            ))
            .or_not(),
        )
        .map_with(|(cond, if_cases), e| {
            if let Some((if_, else_)) = if_cases {
                (
                    Expr::If(Box::new(cond), Box::new(if_), else_.map(Box::new)),
                    e.span(),
                )
            } else {
                cond
            }
        });

    let binary6 = make_binary_op(
        if_,
        select! {
            Token::Symbol("|") => BinaryOp::Pipe,
        },
        false,
    );

    sexpr.define(binary6.labelled("expression").as_context().boxed());

    // Statements

    let expr_or_assign_stmt = group((
        choice((
            just(Token::Kw("export")).to(AssignModifier::Export),
            just(Token::Kw("global")).to(AssignModifier::Global),
            just(Token::Kw("nonlocal")).to(AssignModifier::Nonlocal),
        ))
        .repeated()
        .collect()
        .boxed(),
        nary_tuple.clone(),
        symbol("=").ignore_then(nary_tuple.clone()).or_not(),
    ))
    .map(|(modifiers, lhs, rhs)| {
        if let Some(rhs) = rhs {
            Stmt::Assign(lhs, rhs, modifiers)
        } else {
            Stmt::Expr(lhs, modifiers)
        }
    })
    .boxed();

    let inline_expr_or_assign_stmt = group((
        choice((
            just(Token::Kw("export")).to(AssignModifier::Export),
            just(Token::Kw("global")).to(AssignModifier::Global),
            just(Token::Kw("nonlocal")).to(AssignModifier::Nonlocal),
        ))
        .repeated()
        .collect()
        .boxed(),
        sexpr.clone(),
        symbol("=").ignore_then(sexpr.clone()).or_not(),
    ))
    .map(|(modifiers, lhs, rhs)| {
        if let Some(rhs) = rhs {
            Stmt::Assign(lhs, rhs, modifiers)
        } else {
            Stmt::Expr(lhs, modifiers)
        }
    })
    .boxed();

    let while_stmt = just(Token::Kw("while"))
        .ignore_then(sexpr.clone())
        .then_ignore(just(START_BLOCK))
        .then(block_or_inline_stmt.clone())
        .map(|(cond, body)| Stmt::While(cond, body))
        .labelled("while statement")
        .boxed();

    let except_block = just(Token::Eol)
        .then(just(Token::Kw("except")))
        .ignore_then(
            group((
                except_types.clone(),
                just(Token::Kw("as")).ignore_then(ident.clone()).or_not(),
            ))
            .or_not(),
        )
        .boxed()
        .then(just(START_BLOCK).ignore_then(block_or_inline_stmt.clone()))
        .map(|(opt, body)| {
            if let Some((types, name)) = opt {
                ExceptHandler {
                    types: Some(types),
                    name,
                    body,
                }
            } else {
                ExceptHandler {
                    types: None,
                    name: None,
                    body,
                }
            }
        })
        .labelled("except block")
        .boxed();

    let finally_block = one_of([Token::Eol])
        .then(just(Token::Kw("finally")))
        .then(just(START_BLOCK))
        .ignore_then(block_or_inline_stmt.clone())
        .labelled("finally block")
        .boxed();

    let try_stmt = just(Token::Kw("try"))
        .then(just(START_BLOCK))
        .ignore_then(group((
            block_or_inline_stmt.clone(),
            except_block.repeated().collect(),
            finally_block.or_not(),
        )))
        .map(|(body, excepts, finally)| Stmt::Try(body, excepts, finally))
        .labelled("try statement")
        .boxed();

    let for_stmt = just(Token::Kw("for"))
        .ignore_then(group((
            nary_tuple.clone().then_ignore(just(Token::Kw("in"))),
            sexpr.clone().then_ignore(just(START_BLOCK)),
            block_or_inline_stmt.clone(),
        )))
        .map(|(decl, iter, body)| Stmt::For(decl, iter, body))
        .labelled("for statement")
        .boxed();

    let return_stmt = just(Token::Kw("return"))
        .ignore_then(nary_tuple.clone())
        .map(Stmt::Return)
        .labelled("return statement")
        .boxed();

    let inline_return_stmt = just(Token::Kw("return"))
        .ignore_then(sexpr.clone())
        .map(Stmt::Return)
        .labelled("inline return statement")
        .boxed();

    let assert_stmt = just(Token::Kw("assert"))
        .ignore_then(sexpr.clone())
        .then(symbol(",").ignore_then(sexpr.clone()).or_not())
        .map(|(x, y)| Stmt::Assert(x, y))
        .labelled("assert statement")
        .boxed();

    let raise_stmt = just(Token::Kw("raise"))
        .ignore_then(nary_tuple.clone())
        .map(Stmt::Raise)
        .labelled("raise statement")
        .boxed();

    let inline_raise_stmt = just(Token::Kw("raise"))
        .ignore_then(sexpr.clone())
        .map(Stmt::Raise)
        .labelled("inline raise statement")
        .boxed();

    let break_stmt = just(Token::Kw("break"))
        .map(|_| Stmt::Break)
        .labelled("break statement")
        .boxed();

    let continue_stmt = just(Token::Kw("continue"))
        .map(|_| Stmt::Continue)
        .labelled("continue statement")
        .boxed();

    let import_stmt = just(Token::Kw("export"))
        .to(1)
        .or_not()
        .then_ignore(just(Token::Kw("import")))
        .then(group((
            symbol(".").repeated().count(),
            ident
                .clone()
                .then_ignore(symbol("."))
                .repeated()
                .collect()
                .boxed(),
            choice((
                enumeration(
                    ident
                        .clone()
                        .then(just(Token::Kw("as")).ignore_then(ident.clone()).or_not()),
                    symbol(","),
                )
                .delimited_by_with_eol(symbol("("), symbol(")"))
                .map(ImportList::Leaves)
                .boxed(),
                just(Token::Symbol("*")).map(|_| ImportList::Star),
                ident
                    .clone()
                    .then(just(Token::Kw("as")).ignore_then(ident.clone()).or_not())
                    .map(|x| ImportList::Leaves(vec![x]))
                    .boxed(),
            ))
            .boxed(),
        )))
        .map(|(reexport, (level, trunk, import_list))| -> Stmt {
            Stmt::Import(ImportStmt {
                trunk,
                imports: import_list,
                level,
                reexport: reexport.is_some(),
            })
        })
        .labelled("import statement")
        .boxed();

    let module_stmt = just(Token::Kw("module")).map(|_| Stmt::Module);

    stmt.define(
        choice((
            expr_or_assign_stmt.then_ignore(just(Token::Eol)),
            module_stmt.then_ignore(just(Token::Eol)),
            while_stmt.clone().then_ignore(just(Token::Eol)),
            for_stmt.clone().then_ignore(just(Token::Eol)),
            return_stmt.then_ignore(just(Token::Eol)),
            assert_stmt.then_ignore(just(Token::Eol)),
            raise_stmt.then_ignore(just(Token::Eol)),
            break_stmt.clone().then_ignore(just(Token::Eol)),
            continue_stmt.clone().then_ignore(just(Token::Eol)),
            import_stmt.then_ignore(just(Token::Eol)),
            try_stmt.then_ignore(just(Token::Eol)),
        ))
        .labelled("statement")
        .spanned()
        .boxed(),
    );

    inline_stmt.define(
        choice((
            inline_expr_or_assign_stmt,
            while_stmt,
            for_stmt,
            inline_return_stmt,
            inline_raise_stmt,
            break_stmt,
            continue_stmt,
        ))
        .labelled("inline-statement")
        .spanned()
        .boxed(),
    );

    stmts.labelled("program")
}

pub fn parse_tokens<'tokens, 'src: 'tokens>(
    src: &'src str,
    tokens: &'tokens TokenList<'src>,
) -> (Option<SBlock<'src>>, Vec<Rich<'tokens, Token<'src>, Span>>) {
    parser()
        .parse(
            tokens
                .0
                .as_slice()
                // convert the span type with map
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        )
        .into_output_errors()
}
