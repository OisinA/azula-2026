use std::{iter::Peekable, rc::Rc};

use azula_ast::prelude::*;
use azula_error::prelude::*;
use azula_type::prelude::AzulaType;

use crate::{
    prelude::Lexer,
    token::{Token, TokenKind},
};

type OperatorPrecedence = u8;

const LOWEST: OperatorPrecedence = 0;
const OR: OperatorPrecedence = 1;
const AND: OperatorPrecedence = 2;
const EQUALS: OperatorPrecedence = 3;
const LESS_GREATER: OperatorPrecedence = 4;
const BIT_OR: OperatorPrecedence = 5;
const BIT_XOR: OperatorPrecedence = 6;
const BIT_AND: OperatorPrecedence = 7;
const SHIFT: OperatorPrecedence = 8;
const SUM: OperatorPrecedence = 9;
const PRODUCT: OperatorPrecedence = 10;
const PREFIX: OperatorPrecedence = 11;
const CAST: OperatorPrecedence = 12;
const STRUCT_INIT: OperatorPrecedence = 13;
const CALL: OperatorPrecedence = 14;
const ACCESS: OperatorPrecedence = 15;

pub struct Parser<'a> {
    source: &'a str,
    lexer: Peekable<Lexer<'a>>,

    pub errors: Vec<AzulaError>,

    /// Set when a `>>` token closed two levels of generic arguments at once;
    /// the outer level's `>` has then already been consumed.
    pending_greater: bool,

    /// Declarations produced alongside the one just parsed (the methods of a
    /// struct or enum become an impl block)
    extra_items: Vec<Statement<'a>>,
}

impl<'a> Parser<'a> {
    pub fn new(source: &'a str, lexer: Lexer<'a>) -> Self {
        Self {
            source,
            lexer: lexer.peekable(),
            errors: vec![],
            pending_greater: false,
            extra_items: vec![],
        }
    }

    pub fn parse(&mut self) -> Statement<'a> {
        Statement::Root(self.parse_block(TokenKind::EOF))
    }

    fn parse_statement(&mut self) -> Option<Statement<'a>> {
        let token = self.lexer.peek();
        if token.is_none() {
            return None;
        }
        let token = token.unwrap();

        match token.kind {
            TokenKind::Function => self.parse_function(),
            TokenKind::Extern => self.parse_extern_function(),
            TokenKind::Struct => self.parse_struct(),
            TokenKind::Enum => self.parse_enum(),
            TokenKind::Type => self.parse_type_alias(),
            TokenKind::Return => self.parse_return(),
            TokenKind::Var => self.parse_assign(true),
            TokenKind::Const => self.parse_assign(false),
            TokenKind::If => self.parse_if(),
            TokenKind::While => self.parse_while(),
            TokenKind::For => self.parse_for(),
            TokenKind::Break => {
                let tok = self.lexer.next().unwrap();
                Some(Statement::Break(Span { start: tok.span.start, end: tok.span.end }))
            }
            TokenKind::Continue => {
                let tok = self.lexer.next().unwrap();
                Some(Statement::Continue(Span { start: tok.span.start, end: tok.span.end }))
            }
            TokenKind::Impl => {
                let tok = self.lexer.next().unwrap();
                self.errors.push(AzulaError::new(
                    ErrorType::Custom("`impl` blocks are gone: put methods in the type's body, or use `extend`".to_string()),
                    tok.span.start,
                    tok.span.end,
                ));
                None
            }
            TokenKind::Extend => self.parse_extend(),
            TokenKind::SemiColon => {
                self.lexer.next();
                None
            }
            TokenKind::Comment => {
                self.lexer.next();
                None
            }
            TokenKind::Import => self.parse_import(),
            _ => {
                let expr = match self.parse_expression(LOWEST, true) {
                    Some(node) => node,
                    None => return None,
                };

                if self.lexer.peek().unwrap().kind == TokenKind::Assign {
                    return self.parse_reassign(expr.clone());
                }

                // A `match` used as a statement doesn't need a trailing semicolon
                if matches!(expr.expression, Expression::Match(..))
                    && self.lexer.peek().map(|t| t.kind != TokenKind::SemiColon).unwrap_or(true)
                {
                    return Some(Statement::ExpressionStatement(expr.clone(), expr.span));
                }

                if !self.expect_peek(TokenKind::SemiColon) {
                    return None;
                }

                Some(Statement::ExpressionStatement(expr.clone(), expr.span))
            }
        }
    }

    fn parse_block(&mut self, ending: TokenKind) -> Vec<Statement<'a>> {
        let mut tok = self.lexer.peek().is_some();
        let mut statements = vec![];
        while tok {
            if self.lexer.peek().unwrap().kind == ending {
                break;
            }
            let stmt = self.parse_statement();
            tok = self.lexer.peek().is_some();
            if stmt.is_none() {
                // This could cause issues - not sure how it would occur
                continue;
            }
            statements.push(stmt.unwrap());
            statements.append(&mut self.extra_items);
        }
        statements
    }

    fn parse_function(&mut self) -> Option<Statement<'a>> {
        // func
        let start_token = self.lexer.next().unwrap();

        // Parse name of the function
        let tok = self.lexer.next();
        let ident = match tok {
            Some(v) if matches!(v.kind, TokenKind::Identifier(_)) => {
                if let TokenKind::Identifier(val) = v.kind {
                    val
                } else {
                    "anon"
                }
            }
            _ => return None,
        };

        let type_params = self.parse_type_params();

        // Parse function arguments
        let mut args = vec![];
        if let Some(tok) = self.lexer.peek() {
            if tok.kind == TokenKind::BracketOpen {
                args = self.parse_typed_identifier_list(TokenKind::BracketOpen);
            }
        }

        // Parse return type
        let mut returns = AzulaType::Void;
        if let Some(tok) = self.lexer.peek() {
            // If it's a colon, we have a type
            if tok.kind == TokenKind::Colon {
                self.lexer.next();

                returns = self.parse_type();
            }
        }

        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }
        self.lexer.next();

        let body = self.parse_block(TokenKind::BraceClose);

        if !self.expect_peek(TokenKind::BraceClose) {
            return None;
        }

        let end_token = self.lexer.next().unwrap();

        let function = Statement::Function {
            name: ident,
            args,
            returns,
            body: Rc::new(Statement::Block(body)),
            span: Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        };
        if type_params.is_empty() {
            Some(function)
        } else {
            Some(Statement::Generic(type_params, Rc::new(function)))
        }
    }

    fn parse_extern_function(&mut self) -> Option<Statement<'a>> {
        // extern
        let start_token = self.lexer.next().unwrap();

        let next = self.lexer.next().unwrap();

        let varargs = match next.kind {
            TokenKind::VarArgs => {
                self.lexer.next();
                true
            }
            _ => false,
        };

        // Parse name of the function
        let tok = self.lexer.next();
        let ident = match tok {
            Some(v) if matches!(v.kind, TokenKind::Identifier(_)) => {
                if let TokenKind::Identifier(val) = v.kind {
                    val
                } else {
                    "anon"
                }
            }
            _ => return None,
        };

        // Parse function arguments
        let mut args = vec![];
        if let Some(tok) = self.lexer.peek() {
            if tok.kind == TokenKind::BracketOpen {
                args = self.parse_type_list(TokenKind::BracketOpen);
            }
        }

        // Parse return type
        let mut returns = AzulaType::Void;
        if let Some(tok) = self.lexer.peek() {
            // If it's a colon, we have a type
            if tok.kind == TokenKind::Colon {
                self.lexer.next();

                returns = self.parse_type();
            }
        }
        if !self.expect_peek(TokenKind::SemiColon) {
            return None;
        }

        let end_token = self.lexer.next().unwrap();

        Some(Statement::ExternFunction {
            name: ident,
            varargs,
            args,
            returns,
            span: Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        })
    }

    fn parse_struct(&mut self) -> Option<Statement<'a>> {
        // struct
        let start_token = self.lexer.next().unwrap();

        // Parse name of the struct
        let tok = self.lexer.next();
        let ident = match tok {
            Some(v) if matches!(v.kind, TokenKind::Identifier(_)) => {
                if let TokenKind::Identifier(val) = v.kind {
                    val
                } else {
                    "anon"
                }
            }
            _ => return None,
        };

        let type_params = self.parse_type_params();

        // Body: `name: Type;` fields and methods
        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }
        self.lexer.next();
        let mut fields = vec![];
        let mut methods = vec![];
        loop {
            match self.lexer.peek().map(|t| t.kind.clone()) {
                Some(TokenKind::BraceClose) => break,
                Some(TokenKind::Function) => methods.push(self.parse_method()?),
                Some(TokenKind::Identifier(_)) => {
                    let field = self.parse_typed_identifier()?;
                    if !self.expect_peek(TokenKind::SemiColon) {
                        return None;
                    }
                    self.lexer.next();
                    fields.push(field);
                }
                _ => {
                    let tok = self.lexer.next();
                    let (start, end) = tok.map(|t| (t.span.start, t.span.end)).unwrap_or((0, 0));
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken("a field or method".to_string(), None),
                        start,
                        end,
                    ));
                    return None;
                }
            }
        }
        let end_token = self.lexer.next().unwrap();
        let span = Span {
            start: start_token.span.start,
            end: end_token.span.end,
        };
        self.add_methods(ident, &type_params, methods, span.clone());

        let struc = Statement::Struct {
            name: ident,
            attributes: fields,
            span,
        };
        if type_params.is_empty() {
            Some(struc)
        } else {
            Some(Statement::Generic(type_params, Rc::new(struc)))
        }
    }

    /// A function in a type's body. It's a method, receiving `self`, if its body uses `self`.
    fn parse_method(&mut self) -> Option<Statement<'a>> {
        match self.parse_function()? {
            Statement::Function { name, mut args, returns, body, span } => {
                if statement_mentions_self(&body) {
                    args.insert(0, (AzulaType::Named("__self".to_string()), "self"));
                }
                Some(Statement::Function { name, args, returns, body, span })
            }
            other => Some(other),
        }
    }

    /// Record the methods declared in the body of type `name` as an impl block.
    fn add_methods(&mut self, name: &'a str, type_params: &[&'a str], methods: Vec<Statement<'a>>, span: Span) {
        if methods.is_empty() {
            return;
        }
        let target = if type_params.is_empty() {
            AzulaType::Named(name.to_string())
        } else {
            AzulaType::Generic(
                name.to_string(),
                type_params.iter().map(|p| AzulaType::Named(p.to_string())).collect(),
            )
        };
        let imp = Statement::Impl {
            struct_impl: target,
            trait_impl: None,
            funcs: methods,
            span,
        };
        self.extra_items.push(if type_params.is_empty() {
            imp
        } else {
            Statement::Generic(type_params.to_vec(), Rc::new(imp))
        });
    }

    /// `extend Type { methods }`
    fn parse_extend(&mut self) -> Option<Statement<'a>> {
        let start_token = self.lexer.next().unwrap();
        let target = self.parse_type();
        let mut type_params = vec![];
        if let AzulaType::Generic(_, args) = &target {
            for arg in args {
                if let AzulaType::Named(name) = arg {
                    let name: &'a str = Box::leak(name.clone().into_boxed_str());
                    type_params.push(name);
                }
            }
        }
        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }
        self.lexer.next();
        let mut methods = vec![];
        while self.lexer.peek().map(|t| t.kind != TokenKind::BraceClose).unwrap_or(false) {
            if !self.expect_peek(TokenKind::Function) {
                return None;
            }
            methods.push(self.parse_method()?);
        }
        let end_token = self.lexer.next().unwrap();
        let imp = Statement::Impl {
            struct_impl: target,
            trait_impl: None,
            funcs: methods,
            span: Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        };
        if type_params.is_empty() {
            Some(imp)
        } else {
            Some(Statement::Generic(type_params, Rc::new(imp)))
        }
    }

    fn parse_import(&mut self) -> Option<Statement<'a>> {
        let start = self.lexer.next()?.span.start; // consume `import`
        let tok = self.lexer.next()?;
        let path = if let TokenKind::String(s) = tok.kind {
            s.to_string()
        } else {
            self.errors.push(AzulaError::new(
                ErrorType::ExpectedToken("String".to_string(), Some(format!("{:?}", tok.kind))),
                tok.span.start,
                tok.span.end,
            ));
            return None;
        };
        let end = tok.span.end;
        Some(Statement::Import(path, Span { start, end }))
    }

    fn parse_type_alias(&mut self) -> Option<Statement<'a>> {
        let start = self.lexer.next()?.span.start; // consume `type`
        let name = if let TokenKind::Identifier(n) = self.lexer.next()?.kind { n } else { return None; };
        self.expect_peek(TokenKind::Assign);
        self.lexer.next(); // consume =
        let typ = self.parse_type();
        let end = self.lexer.peek().map(|t| t.span.end).unwrap_or(start);
        self.expect_peek(TokenKind::SemiColon);
        self.lexer.next(); // consume ;
        Some(Statement::TypeAlias { name, typ, span: Span { start, end } })
    }

    fn parse_enum(&mut self) -> Option<Statement<'a>> {
        // enum
        let start_token = self.lexer.next().unwrap();

        // Parse name of the enum
        let tok = self.lexer.next();
        let ident = match tok {
            Some(v) if matches!(v.kind, TokenKind::Identifier(_)) => {
                if let TokenKind::Identifier(val) = v.kind {
                    val
                } else {
                    "anon"
                }
            }
            _ => return None,
        };

        let type_params = self.parse_type_params();

        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }
        self.lexer.next();

        // Parse `;`-terminated variants (optionally carrying payload types) and methods
        let mut variants = vec![];
        let mut payloads = vec![];
        let mut methods = vec![];
        while let Some(tok) = self.lexer.peek() {
            if tok.kind == TokenKind::BraceClose {
                break;
            }
            if tok.kind == TokenKind::Function {
                methods.push(self.parse_method()?);
                continue;
            }

            let tok = self.lexer.next().unwrap();
            match tok.kind {
                TokenKind::Identifier(val) => {
                    variants.push(val);
                    if self.lexer.peek().map(|t| t.kind == TokenKind::BracketOpen).unwrap_or(false) {
                        payloads.push(self.parse_type_list(TokenKind::BracketOpen));
                    } else {
                        payloads.push(vec![]);
                    }
                    if !self.expect_peek(TokenKind::SemiColon) {
                        return None;
                    }
                    self.lexer.next();
                }
                _ => {
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken(
                            format!("{:?}", TokenKind::Identifier("")),
                            Some(format!("{:?}", tok.kind)),
                        ),
                        tok.span.start,
                        tok.span.end,
                    ));
                    return None;
                }
            }
        }

        // Consume the closing brace
        let end_token = self.lexer.next().unwrap();
        self.add_methods(
            ident,
            &type_params,
            methods,
            Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        );

        let enm = Statement::Enum {
            name: ident,
            variants,
            payloads,
            span: Span {
                start: start_token.span.start,
                end: start_token.span.end,
            },
        };
        if type_params.is_empty() {
            Some(enm)
        } else {
            Some(Statement::Generic(type_params, Rc::new(enm)))
        }
    }

    fn parse_return(&mut self) -> Option<Statement<'a>> {
        // return
        let start_token = self.lexer.next().unwrap();

        if let Some(v) = self.lexer.peek() {
            if v.kind == TokenKind::SemiColon {
                self.lexer.next();
                Some(Statement::Return(
                    None,
                    Span {
                        start: start_token.span.start,
                        end: start_token.span.end,
                    },
                ))
            } else {
                let expr = if let Some(expr) = self.parse_expression(LOWEST, true) {
                    expr
                } else {
                    return None;
                };
                if !self.expect_peek(TokenKind::SemiColon) {
                    return None;
                }

                let end_token = self.lexer.next().unwrap();
                Some(Statement::Return(
                    Some(expr.clone()),
                    Span {
                        start: start_token.span.start,
                        end: end_token.span.end,
                    },
                ))
            }
        } else {
            Some(Statement::Return(
                None,
                Span {
                    start: start_token.span.start,
                    end: start_token.span.end,
                },
            ))
        }
    }

    fn parse_assign(&mut self, mutable: bool) -> Option<Statement<'a>> {
        // var
        let start_token = self.lexer.next().unwrap();

        let tok = self.lexer.next();
        let ident = match tok {
            Some(v) if matches!(v.kind, TokenKind::Identifier(_)) => {
                if let TokenKind::Identifier(val) = v.kind {
                    val
                } else {
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken(
                            format!("{:?}", TokenKind::Identifier("")),
                            Some(format!("{:?}", v.kind)),
                        ),
                        v.span.start,
                        v.span.end,
                    ));

                    return None;
                }
            }
            _ => return None,
        };

        let mut type_annotation = None;
        if self.lexer.peek().unwrap().kind == TokenKind::Colon {
            self.lexer.next();

            let typ = self.parse_type();
            type_annotation = Some(typ);
        }

        if !self.expect_peek(TokenKind::Assign) {
            return None;
        }

        self.lexer.next();

        let expr = if let Some(expr) = self.parse_expression(LOWEST, true) {
            expr
        } else {
            return None;
        };

        if !self.expect_peek(TokenKind::SemiColon) {
            return None;
        }

        let end_token = self.lexer.next().unwrap();

        Some(Statement::Assign(
            mutable,
            ident.to_string(),
            type_annotation,
            expr,
            Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        ))
    }

    fn parse_reassign(&mut self, ident: ExpressionNode<'a>) -> Option<Statement<'a>> {
        self.lexer.next();

        let expr = if let Some(expr) = self.parse_expression(LOWEST, true) {
            expr
        } else {
            return None;
        };

        if !self.expect_peek(TokenKind::SemiColon) {
            return None;
        }

        let end_token = self.lexer.next().unwrap();

        Some(Statement::Reassign(
            ident.clone(),
            expr,
            Span {
                start: ident.span.start,
                end: end_token.span.end,
            },
        ))
    }

    fn parse_if(&mut self) -> Option<Statement<'a>> {
        // if
        let start_token = self.lexer.next().unwrap();

        let expr = self.parse_expression(LOWEST, false);

        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }

        self.lexer.next();

        let body = self.parse_block(TokenKind::BraceClose);

        let end_token = self.lexer.next().unwrap();

        // Check for optional else / else if
        let else_branch = if let Some(tok) = self.lexer.peek() {
            if tok.kind == TokenKind::Else {
                self.lexer.next(); // consume `else`
                if let Some(next) = self.lexer.peek() {
                    if next.kind == TokenKind::If {
                        // else if — parse another full if statement
                        self.parse_if().map(|s| Rc::new(s))
                    } else {
                        // else { ... }
                        if !self.expect_peek(TokenKind::BraceOpen) {
                            return None;
                        }
                        self.lexer.next();
                        let else_body = self.parse_block(TokenKind::BraceClose);
                        self.lexer.next();
                        Some(Rc::new(Statement::Block(else_body)))
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        Some(Statement::If(
            expr.unwrap(),
            body,
            else_branch,
            Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        ))
    }

    fn parse_while(&mut self) -> Option<Statement<'a>> {
        // while
        let start_token = self.lexer.next().unwrap();

        let expr = self.parse_expression(LOWEST, false);

        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }

        self.lexer.next();

        let body = self.parse_block(TokenKind::BraceClose);

        let end_token = self.lexer.next().unwrap();

        Some(Statement::While(
            expr.unwrap(),
            body,
            Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        ))
    }

    fn parse_for(&mut self) -> Option<Statement<'a>> {
        // for [condition] { ... }
        let start_token = self.lexer.next().unwrap();

        // Infinite loop if next token is `{`
        let cond = if self.lexer.peek().map(|t| t.kind == TokenKind::BraceOpen).unwrap_or(false) {
            None
        } else {
            Some(self.parse_expression(LOWEST, false)?)
        };

        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }

        self.lexer.next();

        let body = self.parse_block(TokenKind::BraceClose);

        let end_token = self.lexer.next().unwrap();

        Some(Statement::For(
            cond,
            body,
            Span {
                start: start_token.span.start,
                end: end_token.span.end,
            },
        ))
    }

    fn parse_type(&mut self) -> AzulaType<'a> {
        if let Some(tok) = self.lexer.next() {
            if let TokenKind::Identifier(ident) = tok.kind {
                let base: AzulaType<'a> = ident.into();
                if matches!(base, AzulaType::Named(_))
                    && self.lexer.peek().map(|t| t.kind == TokenKind::Less).unwrap_or(false)
                {
                    let args = self.parse_type_args();
                    return AzulaType::Generic(ident.to_string(), args);
                }
                return base;
            }

            if let TokenKind::Ampersand = tok.kind {
                return AzulaType::Pointer(Rc::new(self.parse_type()));
            }

            if let TokenKind::SquareOpen = tok.kind {
                let internal_type = self.parse_type();
                let mut size = None;
                if self.lexer.peek().unwrap().kind == TokenKind::SemiColon {
                    self.lexer.next();

                    if let TokenKind::Integer(i) = self.lexer.next().unwrap().kind {
                        size = Some(i as usize);
                    }
                }
                if !self.expect_peek(TokenKind::SquareClose) {
                    return AzulaType::Void;
                }

                self.lexer.next();

                return AzulaType::Array(Rc::new(internal_type), size);
            }

            // Bit of a hack to allow for double &&
            if let TokenKind::And = tok.kind {
                return AzulaType::Pointer(Rc::new(AzulaType::Pointer(Rc::new(self.parse_type()))));
            }
        }

        AzulaType::Void
    }

    /// Parse `<T1, T2, ...>` type arguments (the `<` is the next token).
    fn parse_type_args(&mut self) -> Vec<AzulaType<'a>> {
        self.lexer.next(); // consume <
        let mut args = vec![];
        loop {
            args.push(self.parse_type());
            if self.pending_greater {
                self.pending_greater = false;
                break;
            }
            match self.lexer.peek().map(|t| t.kind.clone()) {
                Some(TokenKind::Comma) => {
                    self.lexer.next();
                }
                Some(TokenKind::Greater) => {
                    self.lexer.next();
                    break;
                }
                Some(TokenKind::ShiftRight) => {
                    // `>>` closes this list and the enclosing one
                    self.lexer.next();
                    self.pending_greater = true;
                    break;
                }
                _ => {
                    self.expect_peek(TokenKind::Greater);
                    break;
                }
            }
        }
        args
    }

    /// Parse `<T, U>` type parameter names if present.
    fn parse_type_params(&mut self) -> Vec<&'a str> {
        let mut params = vec![];
        if self.lexer.peek().map(|t| t.kind != TokenKind::Less).unwrap_or(true) {
            return params;
        }
        self.lexer.next(); // consume <
        while let Some(tok) = self.lexer.next() {
            match tok.kind {
                TokenKind::Identifier(name) => params.push(name),
                TokenKind::Comma => continue,
                TokenKind::Greater => break,
                _ => {
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken("type parameter".to_string(), Some(format!("{:?}", tok.kind))),
                        tok.span.start,
                        tok.span.end,
                    ));
                    break;
                }
            }
        }
        params
    }

    fn parse_typed_identifier(&mut self) -> Option<TypedIdentifier<'a>> {
        let name = if let Some(tok) = self.lexer.next() {
            if let TokenKind::Identifier(ident) = tok.kind {
                ident
            } else {
                self.errors.push(AzulaError::new(
                    ErrorType::ExpectedToken(
                        format!("{:?}", TokenKind::Identifier("")),
                        Some(format!("{:?}", tok.kind)),
                    ),
                    tok.span.start,
                    tok.span.end,
                ));
                return None;
            }
        } else {
            self.errors.push(AzulaError::new(
                ErrorType::UnexpectedEOF,
                self.source.len() - 2,
                self.source.len() - 1,
            ));
            return None;
        };

        if !self.expect_peek(TokenKind::Colon) {
            return None;
        }

        self.lexer.next();

        let ident_type = self.parse_type();

        Some((ident_type, name))
    }

    fn parse_typed_identifier_list(
        &mut self,
        opening_delimiter: TokenKind,
    ) -> Vec<TypedIdentifier<'a>> {
        let closing_delimiter = opening_delimiter.get_closing_delimiter().unwrap();

        self.lexer.next();

        if let Some(peek) = self.lexer.peek() {
            if peek.kind == closing_delimiter {
                self.lexer.next();
                return vec![];
            }
        } else {
            self.errors.push(AzulaError::new(
                ErrorType::UnexpectedEOF,
                self.source.len() - 2,
                self.source.len() - 1,
            ));
            return vec![];
        }

        let mut identifiers = vec![];

        if let Some((typ, name)) = self.parse_typed_identifier() {
            identifiers.push((typ, name));
        }

        let mut peek = self.lexer.peek().unwrap().kind.clone();
        while peek == TokenKind::Comma {
            self.lexer.next();
            if self.lexer.peek().unwrap().kind == closing_delimiter {
                break;
            }
            if let Some((typ, name)) = self.parse_typed_identifier() {
                identifiers.push((typ, name));
            }
            peek = self.lexer.peek().unwrap().kind.clone();
        }

        self.expect_peek(closing_delimiter);

        self.lexer.next();

        identifiers
    }

    fn parse_type_list(&mut self, opening_delimiter: TokenKind) -> Vec<AzulaType<'a>> {
        let closing_delimiter = opening_delimiter.get_closing_delimiter().unwrap();

        // Consume the opening delimiter
        self.lexer.next();

        if let Some(peek) = self.lexer.peek() {
            if peek.kind == closing_delimiter {
                self.lexer.next();
                return vec![];
            }
        } else {
            self.errors.push(AzulaError::new(
                ErrorType::UnexpectedEOF,
                self.source.len() - 2,
                self.source.len() - 1,
            ));
            return vec![];
        }

        let mut types = vec![];

        types.push(self.parse_type());

        let mut peek = self.lexer.peek().unwrap().kind.clone();
        while peek == TokenKind::Comma {
            self.lexer.next();
            types.push(self.parse_type());
            peek = self.lexer.peek().unwrap().kind.clone();
        }

        self.expect_peek(closing_delimiter);

        self.lexer.next();

        types
    }

    fn expect_peek(&mut self, token_kind: TokenKind) -> bool {
        if let Some(tok) = self.lexer.peek() {
            return match tok.kind {
                _ if tok.kind == token_kind => true,
                _ => {
                    let span = tok.span.clone();
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken(
                            format!("{:?}", token_kind),
                            Some(format!("{:?}", tok.kind)),
                        ),
                        span.start,
                        span.end,
                    ));
                    false
                }
            };
        }

        self.errors.push(AzulaError::new(
            ErrorType::UnexpectedEOF,
            self.source.len() - 1,
            self.source.len(),
        ));
        false
    }

    fn parse_expression(
        &mut self,
        precedence: OperatorPrecedence,
        allow_struct_init: bool,
    ) -> Option<ExpressionNode<'a>> {
        let tok = if let Some(tok) = self.lexer.next() {
            tok
        } else {
            self.errors.push(AzulaError::new(
                ErrorType::UnexpectedEOF,
                self.source.len() - 2,
                self.source.len() - 1,
            ));
            return None;
        };

        let mut left = match tok.kind {
            TokenKind::Integer(i) => {
                if let Some(peek) = self.lexer.peek() {
                    if peek.kind == TokenKind::Dot {
                        self.lexer.next();
                        let second_number = self.parse_expression(CALL, false).unwrap();
                        match second_number.expression {
                            Expression::Integer(y) => Some(ExpressionNode {
                                expression: Expression::Float(
                                    format!("{}.{}", i, y).parse().unwrap(),
                                ),
                                typed: AzulaType::Float,
                                span: Span {
                                    start: tok.span.start,
                                    end: second_number.span.end,
                                },
                            }),
                            _ => panic!(),
                        }
                    } else {
                        Some(ExpressionNode {
                            expression: Expression::Integer(i),
                            typed: AzulaType::Int,
                            span: Span {
                                start: tok.span.start,
                                end: tok.span.end,
                            },
                        })
                    }
                } else {
                    Some(ExpressionNode {
                        expression: Expression::Integer(i),
                        typed: AzulaType::Int,
                        span: Span {
                            start: tok.span.start,
                            end: tok.span.end,
                        },
                    })
                }
            }
            TokenKind::True => Some(ExpressionNode {
                expression: Expression::Boolean(true),
                typed: AzulaType::Bool,
                span: Span {
                    start: tok.span.start,
                    end: tok.span.end,
                },
            }),
            TokenKind::False => Some(ExpressionNode {
                expression: Expression::Boolean(false),
                typed: AzulaType::Bool,
                span: Span {
                    start: tok.span.start,
                    end: tok.span.end,
                },
            }),
            TokenKind::Char(s) => {
                let ascii = if s.starts_with('\\') {
                    match s.chars().nth(1) {
                        Some('n') => 10,
                        Some('t') => 9,
                        Some('r') => 13,
                        Some('0') => 0,
                        Some('\\') => 92,
                        Some('\'') => 39,
                        _ => 0,
                    }
                } else {
                    s.chars().next().map(|c| c as i64).unwrap_or(0)
                };
                Some(ExpressionNode {
                    expression: Expression::Integer(ascii),
                    typed: AzulaType::Int,
                    span: Span { start: tok.span.start, end: tok.span.end },
                })
            }
            TokenKind::String(val) => {
                let transformed = match string_transform(val) {
                    Ok(str) => str,
                    Err(index) => {
                        self.errors.push(AzulaError::new(
                            ErrorType::InvalidEscape,
                            tok.span.start + index,
                            tok.span.start + index + 1,
                        ));
                        "".to_string()
                    }
                };
                Some(ExpressionNode {
                    expression: Expression::String(transformed),
                    typed: AzulaType::Str,
                    span: Span {
                        start: tok.span.start,
                        end: tok.span.end,
                    },
                })
            }
            // `new Type { ... }` allocates a struct on the heap
            TokenKind::Identifier("new")
                if matches!(self.lexer.peek().map(|t| &t.kind), Some(TokenKind::Identifier(_))) =>
            {
                let inner = self.parse_expression(PREFIX, true)?;
                if !matches!(inner.expression, Expression::StructInitialisation(..)) {
                    self.errors.push(AzulaError::new(
                        ErrorType::Custom("`new` needs a struct literal".to_string()),
                        tok.span.start,
                        inner.span.end,
                    ));
                    return None;
                }
                Some(ExpressionNode {
                    span: Span { start: tok.span.start, end: inner.span.end },
                    typed: AzulaType::Pointer(Rc::new(inner.typed.clone())),
                    expression: Expression::Alloc(Rc::new(inner)),
                })
            }
            TokenKind::Identifier(x) => Some(ExpressionNode {
                expression: Expression::Identifier(x.to_string()),
                typed: AzulaType::Infer,
                span: Span {
                    start: tok.span.start,
                    end: tok.span.end,
                },
            }),
            TokenKind::BracketOpen => {
                let expr = self.parse_expression(LOWEST, true)?;

                self.expect_peek(TokenKind::BracketClose);

                self.lexer.next();

                Some(expr)
            }
            TokenKind::Bang => {
                let expr = self.parse_expression(PREFIX, allow_struct_init)?;

                Some(ExpressionNode {
                    expression: Expression::Not(Rc::new(expr.clone())),
                    typed: AzulaType::Bool,
                    span: Span {
                        start: tok.span.start,
                        end: expr.span.end,
                    },
                })
            }
            TokenKind::Tilde => {
                let expr = self.parse_expression(PREFIX, allow_struct_init)?;

                Some(ExpressionNode {
                    span: Span {
                        start: tok.span.start,
                        end: expr.span.end,
                    },
                    expression: Expression::BitNot(Rc::new(expr)),
                    typed: AzulaType::Infer,
                })
            }
            TokenKind::SizeOf => {
                if !self.expect_peek(TokenKind::BracketOpen) {
                    return None;
                }
                self.lexer.next(); // consume (
                let typ = self.parse_type();
                if !self.expect_peek(TokenKind::BracketClose) {
                    return None;
                }
                let close = self.lexer.next().unwrap();
                Some(ExpressionNode {
                    expression: Expression::SizeOf(typ),
                    typed: AzulaType::Int,
                    span: Span {
                        start: tok.span.start,
                        end: close.span.end,
                    },
                })
            }
            TokenKind::Minus => {
                let expr = self.parse_expression(PREFIX, allow_struct_init)?;

                Some(ExpressionNode {
                    expression: Expression::Negate(Rc::new(expr.clone())),
                    typed: AzulaType::Infer,
                    span: Span {
                        start: tok.span.start,
                        end: expr.span.end,
                    },
                })
            }
            TokenKind::Ampersand => {
                let expr = self.parse_expression(PREFIX, true)?;

                Some(ExpressionNode {
                    expression: Expression::Pointer(Rc::new(expr.clone())),
                    typed: AzulaType::Infer,
                    span: Span {
                        start: tok.span.start,
                        end: expr.span.end,
                    },
                })
            }
            TokenKind::SquareOpen => self.parse_array(tok),
            TokenKind::Match => self.parse_match(tok),
            TokenKind::Null => Some(ExpressionNode {
                expression: Expression::Null,
                typed: AzulaType::Str,
                span: Span { start: tok.span.start, end: tok.span.end },
            }),
            _ => {
                self.errors.push(AzulaError::new(
                    ErrorType::ExpectedExpression(format!("{:?}", tok.kind)),
                    tok.span.start,
                    tok.span.end,
                ));
                None
            }
        };

        let mut peek_token = &if let Some(tok) = self.lexer.peek() {
            tok
        } else {
            return left;
        }
        .kind;

        // Keep creating infix expressions until precedence is higher or semicolon found
        while peek_token.clone() != TokenKind::SemiColon
            && precedence < operator_precedence(peek_token.clone(), allow_struct_init)
        {
            if left.is_none() {
                return None;
            }
            left = self.parse_infix(left.unwrap(), allow_struct_init);
            peek_token = if let Some(v) = self.lexer.peek() {
                &v.kind
            } else {
                return left;
            };
        }

        left
    }

    fn parse_expression_list(&mut self, opening_delimiter: TokenKind) -> Vec<ExpressionNode<'a>> {
        let closing_delimiter = opening_delimiter.get_closing_delimiter().unwrap();

        self.lexer.next();

        if let Some(peek) = self.lexer.peek() {
            if peek.kind == closing_delimiter {
                return vec![];
            }
        } else {
            self.errors.push(AzulaError::new(
                ErrorType::UnexpectedEOF,
                self.source.len() - 2,
                self.source.len() - 1,
            ));
            return vec![];
        }

        let mut expressions = vec![];

        if let Some(expr) = self.parse_expression(LOWEST, true) {
            expressions.push(expr);
        }

        let mut peek = self.lexer.peek().unwrap().kind.clone();
        while peek == TokenKind::Comma {
            self.lexer.next();
            // Allow a trailing comma
            if self.lexer.peek().map(|t| t.kind == closing_delimiter).unwrap_or(false) {
                break;
            }
            if let Some(expr) = self.parse_expression(LOWEST, true) {
                expressions.push(expr);
            }
            peek = self.lexer.peek().unwrap().kind.clone();
        }

        self.expect_peek(closing_delimiter);

        expressions
    }

    fn parse_infix(
        &mut self,
        left: ExpressionNode<'a>,
        allow_struct_init: bool,
    ) -> Option<ExpressionNode<'a>> {
        let operator = self.lexer.peek().unwrap();
        let kind = operator.kind.clone();

        match kind {
            TokenKind::As => {
                self.lexer.next(); // consume `as`
                let typ = self.parse_type();
                let span = Span { start: left.span.start, end: left.span.end };
                return Some(ExpressionNode {
                    expression: Expression::Cast(Rc::new(left), typ.clone()),
                    typed: typ,
                    span,
                });
            }
            TokenKind::Dot => self.parse_struct_access(left),
            TokenKind::NamespaceAccess => self.parse_namespace_access(left),
            TokenKind::BracketOpen => self.parse_function_call(left),
            TokenKind::SquareOpen => self.parse_array_access(left),
            TokenKind::BraceOpen if allow_struct_init => self.parse_struct_init(left),
            TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Asterisk
            | TokenKind::Slash
            | TokenKind::Modulo
            | TokenKind::Power
            | TokenKind::Or
            | TokenKind::And
            | TokenKind::Equal
            | TokenKind::NotEqual
            | TokenKind::Less
            | TokenKind::LessEqual
            | TokenKind::Greater
            | TokenKind::GreaterEqual
            | TokenKind::Ampersand
            | TokenKind::Bar
            | TokenKind::Caret
            | TokenKind::ShiftLeft
            | TokenKind::ShiftRight => {
                let precedence = operator_precedence(operator.kind.clone(), allow_struct_init);

                self.lexer.next();

                let right = if let Some(expr) = self.parse_expression(precedence, allow_struct_init)
                {
                    expr
                } else {
                    return None;
                };
                let operator = operator_from_token(kind).unwrap();
                return Some(ExpressionNode {
                    expression: Expression::Infix(
                        Rc::new(left.clone()),
                        operator,
                        Rc::new(right.clone()),
                    ),
                    typed: AzulaType::Infer,
                    span: Span {
                        start: left.span.start,
                        end: right.span.end,
                    },
                });
            }
            _ => {
                println!("unknown operator");
                return None;
            }
        }
    }

    fn parse_function_call(&mut self, left: ExpressionNode<'a>) -> Option<ExpressionNode<'a>> {
        let exprs = self.parse_expression_list(TokenKind::BracketOpen);

        let end_token = self.lexer.next().unwrap();

        Some(ExpressionNode {
            expression: Expression::FunctionCall {
                function: Rc::new(left.clone()),
                args: exprs,
            },
            typed: AzulaType::Infer,
            span: Span {
                start: left.span.start,
                end: end_token.span.end,
            },
        })
    }

    fn parse_array(&mut self, tok: Token) -> Option<ExpressionNode<'a>> {
        if let Some(peek) = self.lexer.peek() {
            if peek.kind == TokenKind::SquareClose {
                let close = self.lexer.next().unwrap();
                return Some(ExpressionNode {
                    expression: Expression::Array(vec![]),
                    typed: AzulaType::Array(Rc::new(AzulaType::Infer), Some(0)),
                    span: Span {
                        start: tok.span.start,
                        end: close.span.end,
                    },
                });
            }
        } else {
            self.errors.push(AzulaError::new(
                ErrorType::UnexpectedEOF,
                self.source.len() - 2,
                self.source.len() - 1,
            ));
            return Some(ExpressionNode {
                expression: Expression::Array(vec![]),
                typed: AzulaType::Array(Rc::new(AzulaType::Infer), Some(0)),
                span: Span {
                    start: tok.span.start,
                    end: 0,
                },
            });
        }

        let mut expressions = vec![];

        if let Some(expr) = self.parse_expression(LOWEST, true) {
            expressions.push(expr);
        }

        let mut peek = self.lexer.peek().unwrap().kind.clone();
        // Parse initialiser like [0; 20]
        if peek == TokenKind::SemiColon {
            self.lexer.next();

            if let TokenKind::Integer(v) = self.lexer.next().unwrap().kind {
                for _ in 0..v - 1 {
                    expressions.push(expressions[0].clone());
                }
                self.expect_peek(TokenKind::SquareClose);

                let close = self.lexer.next().unwrap();
                return Some(ExpressionNode {
                    expression: Expression::Array(expressions.clone()),
                    typed: AzulaType::Array(Rc::new(AzulaType::Infer), Some(expressions.len())),
                    span: Span {
                        start: tok.span.start,
                        end: close.span.end,
                    },
                });
            } else {
                self.expect_peek(TokenKind::SquareClose);

                let close = self.lexer.next().unwrap();
                self.errors.push(AzulaError::new(
                    ErrorType::ArrayInitialiserSizeNonConstant,
                    tok.span.start,
                    close.span.end,
                ));
                return None;
            }
        }
        while peek == TokenKind::Comma {
            self.lexer.next();
            if let Some(next_peek) = self.lexer.peek() {
                if next_peek.kind == TokenKind::SquareClose {
                    break;
                }
            }
            if let Some(expr) = self.parse_expression(LOWEST, true) {
                expressions.push(expr);
            }
            peek = self.lexer.peek().unwrap().kind.clone();
        }

        self.expect_peek(TokenKind::SquareClose);

        let close = self.lexer.next().unwrap();

        Some(ExpressionNode {
            expression: Expression::Array(expressions.clone()),
            typed: AzulaType::Array(Rc::new(AzulaType::Infer), Some(expressions.len())),
            span: Span {
                start: tok.span.start,
                end: close.span.end,
            },
        })
    }

    /// Parse a block expression `{ stmts...; final_expr }`.
    /// The opening `{` has already been consumed by the caller;
    /// `start` is its source position (for the resulting span).
    fn parse_block_expr(&mut self, start: usize) -> Option<ExpressionNode<'a>> {
        let mut stmts: Vec<Statement<'a>> = vec![];
        let mut final_expr: Option<ExpressionNode<'a>> = None;

        loop {
            // Skip semicolons and comments
            loop {
                match self.lexer.peek().map(|t| t.kind.clone()) {
                    Some(TokenKind::SemiColon) | Some(TokenKind::Comment) => {
                        self.lexer.next();
                    }
                    _ => break,
                }
            }

            // End of block (void block or trailing semicolon)
            if self
                .lexer
                .peek()
                .map(|t| t.kind == TokenKind::BraceClose || t.kind == TokenKind::EOF)
                .unwrap_or(true)
            {
                break;
            }

            // Statement-starting keywords
            let is_stmt_kw = self
                .lexer
                .peek()
                .map(|t| {
                    matches!(
                        t.kind,
                        TokenKind::Var
                            | TokenKind::Const
                            | TokenKind::If
                            | TokenKind::While
                            | TokenKind::Return
                            | TokenKind::Break
                            | TokenKind::Continue
                            | TokenKind::Function
                            | TokenKind::For
                    )
                })
                .unwrap_or(false);

            if is_stmt_kw {
                if let Some(s) = self.parse_statement() {
                    stmts.push(s);
                }
                continue;
            }

            // Otherwise parse an expression and look at what follows
            let expr = self.parse_expression(LOWEST, true)?;

            match self.lexer.peek().map(|t| t.kind.clone()) {
                Some(TokenKind::Assign) => {
                    // reassignment: expr = rhs;
                    if let Some(s) = self.parse_reassign(expr) {
                        stmts.push(s);
                    }
                }
                Some(TokenKind::SemiColon) => {
                    // expression statement
                    let span = expr.span.clone();
                    self.lexer.next(); // consume ;
                    stmts.push(Statement::ExpressionStatement(expr, span));
                }
                Some(TokenKind::BraceClose) | None => {
                    // final expression — no semicolon
                    final_expr = Some(expr);
                    break;
                }
                Some(_) if matches!(expr.expression, Expression::Match(..)) => {
                    // a match statement followed by more statements
                    let span = expr.span.clone();
                    stmts.push(Statement::ExpressionStatement(expr, span));
                }
                Some(other) => {
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken(
                            "`;` or `}`".to_string(),
                            Some(format!("{:?}", other)),
                        ),
                        self.lexer.peek().map(|t| t.span.start).unwrap_or(0),
                        self.lexer.peek().map(|t| t.span.end).unwrap_or(0),
                    ));
                    return None;
                }
            }
        }

        // Consume the closing `}`
        if !self.expect_peek(TokenKind::BraceClose) {
            return None;
        }
        let close = self.lexer.next().unwrap();

        let typ = if final_expr.is_some() {
            AzulaType::Infer
        } else {
            AzulaType::Void
        };

        Some(ExpressionNode {
            expression: Expression::Block(stmts, final_expr.map(Rc::new)),
            typed: typ,
            span: Span {
                start,
                end: close.span.end,
            },
        })
    }

    fn parse_match(&mut self, tok: Token<'a>) -> Option<ExpressionNode<'a>> {
        let scrutinee = self.parse_expression(LOWEST, false)?;

        if !self.expect_peek(TokenKind::BraceOpen) {
            return None;
        }
        self.lexer.next(); // consume {

        let mut arms = vec![];

        loop {
            // skip trailing commas / whitespace between arms
            while let Some(peek) = self.lexer.peek() {
                if peek.kind == TokenKind::Comma {
                    self.lexer.next();
                } else {
                    break;
                }
            }

            if let Some(peek) = self.lexer.peek() {
                if peek.kind == TokenKind::BraceClose {
                    break;
                }
            } else {
                self.errors.push(AzulaError::new(
                    ErrorType::UnexpectedEOF,
                    self.source.len() - 2,
                    self.source.len() - 1,
                ));
                return None;
            }

            // parse pattern: `_`, integer literal, char literal, or `IDENT::IDENT`
            let pat_tok = self.lexer.next().unwrap();
            let pattern = match &pat_tok.kind {
                TokenKind::Identifier(name) if *name == "_" => MatchPattern::Wildcard,
                TokenKind::Integer(n) => MatchPattern::Integer(*n),
                TokenKind::Minus => match self.lexer.next().map(|t| t.kind) {
                    Some(TokenKind::Integer(n)) => MatchPattern::Integer(-n),
                    _ => {
                        self.errors.push(AzulaError::new(
                            ErrorType::ExpectedExpression("integer".to_string()),
                            pat_tok.span.start,
                            pat_tok.span.end,
                        ));
                        return None;
                    }
                },
                TokenKind::Char(s) => {
                    let ascii = if s.starts_with('\\') {
                        match s.chars().nth(1) {
                            Some('n') => 10,
                            Some('t') => 9,
                            Some('r') => 13,
                            Some('0') => 0,
                            Some('\\') => 92,
                            Some('\'') => 39,
                            _ => 0,
                        }
                    } else {
                        s.chars().next().map(|c| c as i64).unwrap_or(0)
                    };
                    MatchPattern::Integer(ascii)
                }
                TokenKind::Identifier(enum_name) => {
                    // expect ::
                    if !self.expect_peek(TokenKind::NamespaceAccess) {
                        return None;
                    }
                    self.lexer.next(); // consume ::
                    let variant_tok = self.lexer.next().unwrap();
                    match variant_tok.kind {
                        TokenKind::Identifier(variant_name) => {
                            if self.lexer.peek().map(|t| t.kind == TokenKind::BracketOpen).unwrap_or(false) {
                                MatchPattern::Destructure(enum_name, variant_name, self.parse_pattern_bindings()?)
                            } else {
                                MatchPattern::Variant(enum_name, variant_name)
                            }
                        }
                        _ => {
                            self.errors.push(AzulaError::new(
                                ErrorType::ExpectedToken(
                                    "Identifier".to_string(),
                                    Some(format!("{:?}", variant_tok.kind)),
                                ),
                                variant_tok.span.start,
                                variant_tok.span.end,
                            ));
                            return None;
                        }
                    }
                }
                _ => {
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedExpression(format!("{:?}", pat_tok.kind)),
                        pat_tok.span.start,
                        pat_tok.span.end,
                    ));
                    return None;
                }
            };

            // expect =>
            if !self.expect_peek(TokenKind::FatArrow) {
                return None;
            }
            self.lexer.next(); // consume =>

            // parse arm body expression: either a block `{ stmts; final_expr }` or a single expr
            let body = if self
                .lexer
                .peek()
                .map(|t| t.kind == TokenKind::BraceOpen)
                .unwrap_or(false)
            {
                let open = self.lexer.peek().unwrap().span.start;
                self.lexer.next(); // consume {
                self.parse_block_expr(open)?
            } else {
                self.parse_expression(LOWEST, false)?
            };

            arms.push((pattern, body));
        }

        let close = self.lexer.next().unwrap(); // consume }

        Some(ExpressionNode {
            expression: Expression::Match(Rc::new(scrutinee), arms),
            typed: AzulaType::Infer,
            span: Span {
                start: tok.span.start,
                end: close.span.end,
            },
        })
    }

    /// Parse `(a, _, c)` after a variant name in a match pattern.
    fn parse_pattern_bindings(&mut self) -> Option<Vec<Option<&'a str>>> {
        self.lexer.next(); // consume (
        let mut bindings = vec![];
        loop {
            let tok = self.lexer.next()?;
            match tok.kind {
                TokenKind::BracketClose => break,
                TokenKind::Comma => continue,
                TokenKind::Identifier("_") => bindings.push(None),
                TokenKind::Identifier(name) => bindings.push(Some(name)),
                _ => {
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken("identifier".to_string(), Some(format!("{:?}", tok.kind))),
                        tok.span.start,
                        tok.span.end,
                    ));
                    return None;
                }
            }
        }
        Some(bindings)
    }

    fn parse_array_access(&mut self, left: ExpressionNode<'a>) -> Option<ExpressionNode<'a>> {
        self.lexer.next();
        let index = match self.parse_expression(LOWEST, true) {
            Some(expr) => expr,
            None => return None,
        };

        if !self.expect_peek(TokenKind::SquareClose) {
            return None;
        }

        let end_token = self.lexer.next().unwrap();

        Some(ExpressionNode {
            expression: Expression::ArrayAccess(Rc::new(left.clone()), Rc::new(index)),
            typed: AzulaType::Infer,
            span: Span {
                start: left.span.start,
                end: end_token.span.end,
            },
        })
    }

    fn parse_struct_access(&mut self, left: ExpressionNode<'a>) -> Option<ExpressionNode<'a>> {
        self.lexer.next();
        let index = match self.parse_expression(ACCESS, false) {
            Some(expr) => expr,
            None => return None,
        };

        Some(ExpressionNode {
            expression: Expression::StructAccess(Rc::new(left.clone()), Rc::new(index.clone())),
            typed: AzulaType::Infer,
            span: Span {
                start: left.span.start,
                end: index.span.end,
            },
        })
    }

    fn parse_namespace_access(&mut self, left: ExpressionNode<'a>) -> Option<ExpressionNode<'a>> {
        self.lexer.next();

        // Turbofish: `Name::<Types>`
        if self.lexer.peek().map(|t| t.kind == TokenKind::Less).unwrap_or(false) {
            let name = match &left.expression {
                Expression::Identifier(name) => name.clone(),
                _ => {
                    self.errors.push(AzulaError::new(
                        ErrorType::Custom("type arguments can only follow a name".to_string()),
                        left.span.start,
                        left.span.end,
                    ));
                    return None;
                }
            };
            let args = self.parse_type_args();
            return Some(ExpressionNode {
                expression: Expression::Turbofish(name, args),
                typed: AzulaType::Infer,
                span: left.span.clone(),
            });
        }

        let index = match self.parse_expression(ACCESS, false) {
            Some(expr) => expr,
            None => return None,
        };

        Some(ExpressionNode {
            expression: Expression::NamespaceAccess(Rc::new(left.clone()), Rc::new(index.clone())),
            typed: AzulaType::Infer,
            span: Span {
                start: left.span.start,
                end: index.span.end,
            },
        })
    }

    fn parse_struct_init(&mut self, left: ExpressionNode<'a>) -> Option<ExpressionNode<'a>> {
        // {
        self.lexer.next();

        let mut attrs = vec![];

        let mut peek = match self.lexer.peek() {
            Some(peek) => peek.kind.clone(),
            None => {
                self.errors.push(AzulaError::new(
                    ErrorType::UnexpectedEOF,
                    self.source.len() - 2,
                    self.source.len() - 1,
                ));
                return None;
            }
        };

        while peek != TokenKind::BraceClose {
            if let Some(tok) = self.lexer.peek() {
                if tok.kind == TokenKind::Comma {
                    self.lexer.next();
                    if let Some(tok) = self.lexer.peek() {
                        if tok.kind == TokenKind::BraceClose {
                            break;
                        }
                    }
                }
            }
            let tok = self.lexer.next();
            let name = match tok {
                Some(Token {
                    kind: TokenKind::Identifier(val),
                    span: _,
                }) => val,
                Some(tok) => {
                    self.errors.push(AzulaError::new(
                        ErrorType::ExpectedToken(
                            "identifier".to_string(),
                            Some(format!("{:?}", tok.kind)),
                        ),
                        tok.span.start,
                        tok.span.end,
                    ));
                    return None;
                }
                None => {
                    self.errors.push(AzulaError::new(
                        ErrorType::UnexpectedEOF,
                        self.source.len() - 1,
                        self.source.len(),
                    ));
                    return None;
                }
            };

            if !self.expect_peek(TokenKind::Colon) {
                return None;
            }

            self.lexer.next();

            let expr = self.parse_expression(LOWEST, true)?;

            attrs.push((name, expr));

            peek = match self.lexer.peek() {
                Some(peek) => peek.kind.clone(),
                None => {
                    self.errors.push(AzulaError::new(
                        ErrorType::UnexpectedEOF,
                        self.source.len() - 2,
                        self.source.len() - 1,
                    ));
                    return None;
                }
            };
        }

        if !self.expect_peek(TokenKind::BraceClose) {
            return None;
        }

        let end_token = self.lexer.next().unwrap();

        Some(ExpressionNode {
            expression: Expression::StructInitialisation(Rc::new(left.clone()), attrs),
            typed: AzulaType::Infer,
            span: Span {
                start: left.span.start,
                end: end_token.span.end,
            },
        })
    }
}

/// Whether `self` is used anywhere in a statement (to tell methods from static functions)
fn statement_mentions_self(stmt: &Statement) -> bool {
    match stmt {
        Statement::Root(body) | Statement::Block(body) => body.iter().any(statement_mentions_self),
        Statement::Function { body, .. } => statement_mentions_self(body),
        Statement::Return(value, _) => value.as_ref().map(expression_mentions_self).unwrap_or(false),
        Statement::Assign(_, _, _, value, _) => expression_mentions_self(value),
        Statement::ExpressionStatement(e, _) => expression_mentions_self(e),
        Statement::If(cond, body, else_branch, _) => {
            expression_mentions_self(cond)
                || body.iter().any(statement_mentions_self)
                || else_branch.as_ref().map(|e| statement_mentions_self(e)).unwrap_or(false)
        }
        Statement::Reassign(target, value, _) => expression_mentions_self(target) || expression_mentions_self(value),
        Statement::While(cond, body, _) => expression_mentions_self(cond) || body.iter().any(statement_mentions_self),
        Statement::For(cond, body, _) => {
            cond.as_ref().map(expression_mentions_self).unwrap_or(false) || body.iter().any(statement_mentions_self)
        }
        _ => false,
    }
}

fn expression_mentions_self(expr: &ExpressionNode) -> bool {
    let e = |x: &Rc<ExpressionNode>| expression_mentions_self(x);
    match &expr.expression {
        Expression::Identifier(name) => name == "self",
        Expression::Infix(l, _, r) => e(l) || e(r),
        Expression::FunctionCall { function, args } => e(function) || args.iter().any(expression_mentions_self),
        Expression::Not(x) | Expression::BitNot(x) | Expression::Negate(x) | Expression::Pointer(x) | Expression::Alloc(x) => e(x),
        Expression::Cast(x, _) => e(x),
        Expression::Array(items) => items.iter().any(expression_mentions_self),
        Expression::ArrayAccess(a, i) => e(a) || e(i),
        Expression::StructInitialisation(_, fields) => fields.iter().any(|(_, v)| expression_mentions_self(v)),
        // Only the object of a field access or path can be `self`
        Expression::StructAccess(obj, _) => e(obj),
        Expression::NamespaceAccess(ns, _) => e(ns),
        Expression::Match(scrutinee, arms) => e(scrutinee) || arms.iter().any(|(_, body)| expression_mentions_self(body)),
        Expression::Block(stmts, value) => {
            stmts.iter().any(statement_mentions_self) || value.as_ref().map(|v| e(v)).unwrap_or(false)
        }
        _ => false,
    }
}

fn string_transform(str: &str) -> Result<String, usize> {
    let mut result = String::new();

    let mut chars = str.chars().enumerate().peekable();

    loop {
        let char = chars.next();
        if char.is_none() {
            break;
        }
        let (index, char) = char.unwrap();
        if char == '\\' {
            match chars.peek() {
                Some((_, '\\')) => {
                    chars.next();
                    result.push('\\');
                }
                Some((_, 'n')) => {
                    chars.next();
                    result.push('\n');
                }
                Some((_, 't')) => {
                    chars.next();
                    result.push('\t');
                }
                Some((_, 'r')) => {
                    chars.next();
                    result.push('\r');
                }
                Some((_, '0')) => {
                    chars.next();
                    result.push('\0');
                }
                Some((_, '"')) => {
                    chars.next();
                    result.push('"');
                }
                Some((_, '\'')) => {
                    chars.next();
                    result.push('\'');
                }
                Some((_, 'x')) => {
                    chars.next();
                    let (_, char1) = chars.next().unwrap();
                    let (_, char2) = chars.next().unwrap();
                    result.push(
                        char::from_u32(
                            i64::from_str_radix(&format!("{}{}", char1, char2), 16).unwrap() as u32,
                        )
                        .unwrap(),
                    );
                }
                _ => return Err(index + 1),
            }
        } else {
            result.push(char);
        }
    }

    Ok(result)
}

fn operator_from_token(tok: TokenKind) -> Option<Operator> {
    match tok {
        TokenKind::Plus => Some(Operator::Add),
        TokenKind::Minus => Some(Operator::Sub),
        TokenKind::Asterisk => Some(Operator::Mul),
        TokenKind::Slash => Some(Operator::Div),
        TokenKind::Modulo => Some(Operator::Mod),
        TokenKind::Power => Some(Operator::Power),
        TokenKind::Or => Some(Operator::Or),
        TokenKind::And => Some(Operator::And),
        TokenKind::Equal => Some(Operator::Eq),
        TokenKind::NotEqual => Some(Operator::Neq),
        TokenKind::Less => Some(Operator::Lt),
        TokenKind::LessEqual => Some(Operator::Lte),
        TokenKind::Greater => Some(Operator::Gt),
        TokenKind::GreaterEqual => Some(Operator::Gte),
        TokenKind::Ampersand => Some(Operator::BitAnd),
        TokenKind::Bar => Some(Operator::BitOr),
        TokenKind::Caret => Some(Operator::BitXor),
        TokenKind::ShiftLeft => Some(Operator::Shl),
        TokenKind::ShiftRight => Some(Operator::Shr),
        _ => None,
    }
}

fn operator_precedence(tok: TokenKind, allow_struct_init: bool) -> OperatorPrecedence {
    match tok {
        TokenKind::Or => OR,
        TokenKind::And => AND,
        TokenKind::Equal | TokenKind::NotEqual => EQUALS,
        TokenKind::Less | TokenKind::LessEqual | TokenKind::Greater | TokenKind::GreaterEqual => {
            LESS_GREATER
        }
        TokenKind::Bar => BIT_OR,
        TokenKind::Caret => BIT_XOR,
        TokenKind::Ampersand => BIT_AND,
        TokenKind::ShiftLeft | TokenKind::ShiftRight => SHIFT,
        TokenKind::Plus | TokenKind::Minus => SUM,
        TokenKind::Slash | TokenKind::Asterisk | TokenKind::Power | TokenKind::Modulo => PRODUCT,
        TokenKind::As => CAST,
        TokenKind::BraceOpen if allow_struct_init => STRUCT_INIT,
        TokenKind::BracketOpen | TokenKind::SquareOpen => CALL,
        TokenKind::Dot | TokenKind::NamespaceAccess => ACCESS,
        _ => LOWEST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! parser_tests {
        ($($name:ident: $value:expr,)*) => {
            $(
                #[test]
                fn $name() {
                    let (input, expected) = $value;
                    let lexer: Lexer = input.into();
                    let mut parser = Parser::new(input, lexer);
                    if let Statement::Root(body) = parser.parse() {
                        for (i, statement) in body.into_iter().enumerate() {
                            assert_eq!(statement, expected[i])
                        }
                    };
                }
            )*
        }
    }

    parser_tests!(
        function: (
            "func test(x: bool): int { return 5; } func test2(x: int): int { return x; }",
            vec![Statement::Function {
                name: "test",
                args: vec![(AzulaType::Bool, "x")],
                returns: AzulaType::Int,
                body: Rc::new(Statement::Block(vec![Statement::Return(Some(ExpressionNode {
                    expression: Expression::Integer(5),
                    typed: AzulaType::Int,
                    span: Span { start: 33, end: 34},
                }), Span { start: 26, end: 35})])),
                span: Span { start: 0, end: 37},
            }, Statement::Function {
                name: "test2",
                args: vec![(AzulaType::Int, "x")],
                returns: AzulaType::Int,
                body: Rc::new(Statement::Block(vec![Statement::Return(Some(ExpressionNode {
                    expression: Expression::Identifier("x".to_string()),
                    typed: AzulaType::Infer,
                    span: Span { start: 71, end: 72},
                }), Span { start: 64, end: 73},)])),
                span: Span { start: 38, end: 75},
            }],
        ),
        return_int: (
            "return 5",
            vec![Statement::Return(Some(ExpressionNode {
                expression: Expression::Integer(5),
                typed: AzulaType::Int,
                span: Span { start: 7, end: 8},
            }), Span{start: 0, end: 8})],
        ),
        return_none: (
            "return;",
            vec![Statement::Return(None, Span{start: 0, end: 6})],
        ),
    );

    #[test]
    fn test_parse_function() {
        // No args, no return
        let input = "func main { }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_function().unwrap();
        assert!(matches!(func, Statement::Function { .. }));
        if let Statement::Function {
            name,
            args,
            returns,
            body,
            ..
        } = func
        {
            assert_eq!(name, "main");
            assert_eq!(args, vec![]);
            assert_eq!(returns, AzulaType::Void);
            assert_eq!(body, Rc::new(Statement::Block(vec![])));
        }

        // No args, no return
        let input = "func main() { }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_function();
        println!("{:?}", parser.errors);

        let func = func.unwrap();
        assert!(matches!(func, Statement::Function { .. }));
        if let Statement::Function {
            name,
            args,
            returns,
            body,
            ..
        } = func
        {
            assert_eq!(name, "main");
            assert_eq!(args, vec![]);
            assert_eq!(returns, AzulaType::Void);
            assert_eq!(body, Rc::new(Statement::Block(vec![])));
        }

        // Return type
        let input = "func test: bool { }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_function().unwrap();
        assert!(matches!(func, Statement::Function { .. }));
        if let Statement::Function {
            name,
            args,
            returns,
            body,
            ..
        } = func
        {
            assert_eq!(name, "test");
            assert_eq!(args, vec![]);
            assert_eq!(returns, AzulaType::Bool);
            assert_eq!(body, Rc::new(Statement::Block(vec![])));
        }

        // Args & return type
        let input = "func test(x: int, y: bool): bool { }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_function().unwrap();
        assert!(matches!(func, Statement::Function { .. }));
        if let Statement::Function {
            name,
            args,
            returns,
            body,
            ..
        } = func
        {
            assert_eq!(name, "test");
            assert_eq!(args, vec![(AzulaType::Int, "x"), (AzulaType::Bool, "y")]);
            assert_eq!(returns, AzulaType::Bool);
            assert_eq!(body, Rc::new(Statement::Block(vec![])));
        }
    }

    #[test]
    fn test_parse_extern_function() {
        let input = "extern varargs func test(int): bool;";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_statement().unwrap();
        println!("{:?}", func);
        assert!(matches!(func, Statement::ExternFunction { .. }));
        if let Statement::ExternFunction {
            name,
            varargs,
            args,
            returns,
            ..
        } = func
        {
            assert_eq!(name, "test");
            assert_eq!(varargs, true);
            assert_eq!(args, vec![AzulaType::Int]);
            assert_eq!(returns, AzulaType::Bool);
        }
    }

    #[test]
    fn test_parse_assign() {
        let input = "var test = 5;";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_statement().unwrap();
        assert!(matches!(func, Statement::Assign(..)));

        let input = "const test = 5;";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_statement().unwrap();
        assert!(matches!(func, Statement::Assign(..)));

        // Type annotation
        let input = "var test: int = 5;";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let assign = parser.parse_assign(true).unwrap();
        assert!(matches!(func, Statement::Assign(..)));
        if let Statement::Assign(mutable, name, type_annotation, value, ..) = assign {
            assert_eq!(mutable, true);
            assert_eq!(name, "test".to_string());
            assert_eq!(type_annotation, Some(AzulaType::Int));
            assert_eq!(
                value,
                ExpressionNode {
                    expression: Expression::Integer(5),
                    typed: AzulaType::Int,
                    span: Span { start: 16, end: 17 }
                }
            )
        }
    }

    #[test]
    fn test_parse_struct() {
        // No attributes
        let input = "struct Test { }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let struc = parser.parse_statement().unwrap();
        assert!(matches!(struc, Statement::Struct { .. }));

        // Single attribute
        let input = "struct Test { test: int; }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let struc = parser.parse_statement().unwrap();
        assert!(matches!(struc, Statement::Struct { .. }));
        if let Statement::Struct {
            name,
            attributes,
            span,
        } = struc
        {
            assert_eq!(name, "Test");
            assert_eq!(attributes, vec![(AzulaType::Int, "test")]);
        }

        // Multiple attribute
        let input = "struct Test { test: int; test1: &str; }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let struc = parser.parse_statement().unwrap();
        assert!(matches!(struc, Statement::Struct { .. }));
        if let Statement::Struct {
            name,
            attributes,
            span,
        } = struc
        {
            assert_eq!(name, "Test");
            assert_eq!(
                attributes,
                vec![
                    (AzulaType::Int, "test"),
                    (AzulaType::Pointer(Rc::new(AzulaType::Str)), "test1")
                ]
            );
        }
    }

    #[test]
    fn test_parse_if() {
        let input = "if x { return 5; }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_if().unwrap();
        assert!(matches!(func, Statement::If(..)));
        if let Statement::If(expr, body, ..) = func {
            assert_eq!(expr.expression, Expression::Identifier("x".to_string()));
            assert_eq!(
                body[0],
                Statement::Return(
                    Some(ExpressionNode {
                        expression: Expression::Integer(5),
                        typed: AzulaType::Int,
                        span: Span { start: 14, end: 15 }
                    }),
                    Span { start: 7, end: 16 }
                )
            );
        }

        let input = "if x || y { return 5; }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let func = parser.parse_if();
        println!("{:?}", parser.errors);
        let func = func.unwrap();
        assert!(matches!(func, Statement::If(..)));
        if let Statement::If(expr, body, ..) = func {
            assert_eq!(
                expr.expression,
                Expression::Infix(
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("x".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 3, end: 4 },
                    }),
                    Operator::Or,
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("y".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 8, end: 9 },
                    })
                )
            );
            assert_eq!(
                body[0],
                Statement::Return(
                    Some(ExpressionNode {
                        expression: Expression::Integer(5),
                        typed: AzulaType::Int,
                        span: Span { start: 19, end: 20 }
                    }),
                    Span { start: 12, end: 21 }
                )
            );
        }
    }

    #[test]
    fn test_parse_type() {
        // Basic type
        let input = "int";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let typ = parser.parse_type();
        assert_eq!(typ, AzulaType::Int);

        // Pointer
        let input = "&str";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let typ = parser.parse_type();
        assert_eq!(typ, AzulaType::Pointer(Rc::new(AzulaType::Str)));

        // Nested Pointers
        let input = "&&&str";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let typ = parser.parse_type();
        assert_eq!(
            typ,
            AzulaType::Pointer(Rc::new(AzulaType::Pointer(Rc::new(AzulaType::Pointer(
                Rc::new(AzulaType::Str)
            )))))
        );

        // Array
        let input = "[int]";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let typ = parser.parse_type();
        assert_eq!(typ, AzulaType::Array(Rc::new(AzulaType::Int), None));

        // Nested Array
        let input = "[[int]]";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let typ = parser.parse_type();
        assert_eq!(
            typ,
            AzulaType::Array(
                Rc::new(AzulaType::Array(Rc::new(AzulaType::Int), None)),
                None
            )
        );

        // Sized Array
        let input = "[int; 20]";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let typ = parser.parse_type();
        assert_eq!(typ, AzulaType::Array(Rc::new(AzulaType::Int), Some(20)));
    }

    #[test]
    fn test_parse_reassign() {
        // Basic int
        let input = "x = 5;";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let stmt = parser.parse_statement().unwrap();
        assert_eq!(
            stmt,
            Statement::Reassign(
                ExpressionNode {
                    expression: Expression::Identifier("x".to_string()),
                    typed: AzulaType::Infer,
                    span: Span { start: 0, end: 1 }
                },
                ExpressionNode {
                    expression: Expression::Integer(5),
                    typed: AzulaType::Int,
                    span: Span { start: 4, end: 5 }
                },
                Span { start: 0, end: 6 },
            )
        );
    }

    #[test]
    fn test_parse_while() {
        // Basic int
        let input = "while x { i = 1; }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let stmt = parser.parse_statement().unwrap();
        assert_eq!(
            stmt,
            Statement::While(
                ExpressionNode {
                    expression: Expression::Identifier("x".to_string()),
                    typed: AzulaType::Infer,
                    span: Span { start: 6, end: 7 }
                },
                vec![Statement::Reassign(
                    ExpressionNode {
                        expression: Expression::Identifier("i".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 10, end: 11 },
                    },
                    ExpressionNode {
                        expression: Expression::Integer(1),
                        typed: AzulaType::Int,
                        span: Span { start: 14, end: 15 },
                    },
                    Span { start: 10, end: 16 }
                )],
                Span { start: 0, end: 18 },
            )
        );
    }

    #[test]
    fn test_parse_extend() {
        let input = "extend Test { func test { } func get(): int { return self.x; } }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let stmt = parser.parse_statement().unwrap();
        if let Statement::Impl { struct_impl, funcs, .. } = stmt {
            assert_eq!(struct_impl, AzulaType::Named("Test".to_string()));
            assert_eq!(funcs.len(), 2);
            // Only the method that uses `self` receives it
            match (&funcs[0], &funcs[1]) {
                (Statement::Function { args: a, .. }, Statement::Function { args: b, .. }) => {
                    assert!(a.is_empty());
                    assert_eq!(b[0].1, "self");
                }
                _ => panic!("expected functions"),
            }
        } else {
            panic!("expected an impl");
        }
    }

    #[test]
    fn test_struct_methods() {
        let input = "struct Test { x: int; func get(): int { return self.x; } }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);
        if let Statement::Root(items) = parser.parse() {
            assert!(matches!(items[0], Statement::Struct { .. }));
            assert!(matches!(items[1], Statement::Impl { .. }));
        } else {
            panic!("expected a root");
        }
    }

    #[test]
    fn test_parse_array_access() {
        // Basic int
        let input = "x[10]";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expr = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expr,
            ExpressionNode {
                expression: Expression::ArrayAccess(
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("x".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 0, end: 1 }
                    }),
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(10),
                        typed: AzulaType::Int,
                        span: Span { start: 2, end: 4 }
                    })
                ),
                typed: AzulaType::Infer,
                span: Span { start: 0, end: 5 }
            }
        );
    }

    #[test]
    fn test_parse_struct_access() {
        // Basic int
        let input = "test.test";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expr = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expr,
            ExpressionNode {
                expression: Expression::StructAccess(
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("test".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 0, end: 4 }
                    }),
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("test".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 5, end: 9 }
                    })
                ),
                typed: AzulaType::Infer,
                span: Span { start: 0, end: 9 }
            }
        );
    }

    #[test]
    fn test_parse_namespace_access() {
        let input = "Test::test";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expr = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expr,
            ExpressionNode {
                expression: Expression::NamespaceAccess(
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("Test".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 0, end: 4 }
                    }),
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("test".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 6, end: 10 }
                    })
                ),
                typed: AzulaType::Infer,
                span: Span { start: 0, end: 10 }
            }
        );
    }

    #[test]
    fn test_parse_struct_init() {
        // Basic struct init
        let input = "Test { test: 5 }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expr = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expr,
            ExpressionNode {
                expression: Expression::StructInitialisation(
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("Test".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 0, end: 4 }
                    }),
                    vec![(
                        "test",
                        ExpressionNode {
                            expression: Expression::Integer(5),
                            typed: AzulaType::Int,
                            span: Span { start: 13, end: 14 },
                        }
                    )]
                ),
                typed: AzulaType::Infer,
                span: Span { start: 0, end: 16 }
            }
        );

        // Multiple args
        let input = "Test { test: 5, test1: true }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expr = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expr,
            ExpressionNode {
                expression: Expression::StructInitialisation(
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("Test".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 0, end: 4 }
                    }),
                    vec![
                        (
                            "test",
                            ExpressionNode {
                                expression: Expression::Integer(5),
                                typed: AzulaType::Int,
                                span: Span { start: 13, end: 14 },
                            }
                        ),
                        (
                            "test1",
                            ExpressionNode {
                                expression: Expression::Boolean(true),
                                typed: AzulaType::Bool,
                                span: Span { start: 23, end: 27 },
                            }
                        )
                    ]
                ),
                typed: AzulaType::Infer,
                span: Span { start: 0, end: 29 }
            }
        );
    }

    #[test]
    fn test_parse_typed_identifier() {
        // Basic int
        let input = "x: int";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let (typ, name) = parser.parse_typed_identifier().unwrap();
        assert_eq!(typ, AzulaType::Int);
        assert_eq!(name, "x");

        // Custom struct type
        let input = "my_typed_identifier: CustomStruct";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let (typ, name) = parser.parse_typed_identifier().unwrap();
        assert_eq!(typ, AzulaType::Named("CustomStruct".to_string()));
        assert_eq!(name, "my_typed_identifier");

        // Non identifier for name
        let input = "4: CustomStruct";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        parser.parse_typed_identifier();
        assert_eq!(parser.errors.len(), 1);
        assert!(matches!(
            parser.errors[0].error_type,
            ErrorType::ExpectedToken(..)
        ));
        if let ErrorType::ExpectedToken(expected, ..) = &parser.errors[0].error_type {
            assert_eq!(expected.clone(), "Identifier(\"\")".to_string());
        }

        // No colon - EOF
        let input = "x";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        parser.parse_typed_identifier();
        assert_eq!(parser.errors.len(), 1);
        assert!(matches!(
            parser.errors[0].error_type,
            ErrorType::UnexpectedEOF,
        ));

        // Pointer
        let input = "x: &str";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let (typ, name) = parser.parse_typed_identifier().unwrap();
        assert_eq!(parser.errors.len(), 0);
        assert_eq!(typ, AzulaType::Pointer(Rc::new(AzulaType::Str)));
        assert_eq!(name, "x");
    }

    #[test]
    fn test_parse_typed_identifier_list() {
        let input = "(x: int, y: int, z: bool)";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let identifiers = parser.parse_typed_identifier_list(TokenKind::BracketOpen);

        let (typ, name) = &identifiers[0];
        assert_eq!(typ.clone(), AzulaType::Int);
        assert_eq!(name.clone(), "x");

        let (typ, name) = &identifiers[1];
        assert_eq!(typ.clone(), AzulaType::Int);
        assert_eq!(name.clone(), "y");

        let (typ, name) = &identifiers[2];
        assert_eq!(typ.clone(), AzulaType::Bool);
        assert_eq!(name.clone(), "z");
    }

    #[test]
    fn test_parse_expression_list() {
        let input = "(5, \"test\", x, true)";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let identifiers = parser.parse_expression_list(TokenKind::BracketOpen);

        println!("{:?}", parser.errors);

        let num = &identifiers[0];
        assert_eq!(num.typed.clone(), AzulaType::Int);
        matches!(num.expression, Expression::Integer(5));

        let num = &identifiers[1];
        assert_eq!(num.typed.clone(), AzulaType::Str);
        assert_eq!(num.expression, Expression::String("test".to_string()));

        let num = &identifiers[2];
        assert_eq!(num.typed.clone(), AzulaType::Infer);
        assert_eq!(num.expression, Expression::Identifier("x".to_string()));

        let num = &identifiers[3];
        assert_eq!(num.typed.clone(), AzulaType::Bool);
        matches!(num.expression, Expression::Boolean(true));
    }

    #[test]
    fn test_parse_integer_expression() {
        let input = "123";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(matches!(expression.expression, Expression::Integer(123)));
    }

    #[test]
    fn test_parse_boolean_expression() {
        let input = "true";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(matches!(expression.expression, Expression::Boolean(true)));

        let input = "false";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(matches!(expression.expression, Expression::Boolean(false)));
    }

    #[test]
    fn test_parse_infix_expression() {
        // Addition
        let input = "123 + 10";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap().expression;
        assert_eq!(
            expression,
            Expression::Infix(
                Rc::new(ExpressionNode {
                    expression: Expression::Integer(123),
                    typed: AzulaType::Int,
                    span: Span { start: 0, end: 3 },
                }),
                Operator::Add,
                Rc::new(ExpressionNode {
                    expression: Expression::Integer(10),
                    typed: AzulaType::Int,
                    span: Span { start: 6, end: 8 },
                })
            )
        );

        // Precedence
        let input = "123 * 3 + 10 * 5";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap().expression;
        assert_eq!(
            expression,
            Expression::Infix(
                Rc::new(ExpressionNode {
                    expression: Expression::Infix(
                        Rc::new(ExpressionNode {
                            expression: Expression::Integer(123),
                            typed: AzulaType::Int,
                            span: Span { start: 0, end: 3 },
                        }),
                        Operator::Mul,
                        Rc::new(ExpressionNode {
                            expression: Expression::Integer(3),
                            typed: AzulaType::Int,
                            span: Span { start: 6, end: 7 },
                        })
                    ),
                    typed: AzulaType::Infer,
                    span: Span { start: 0, end: 7 },
                }),
                Operator::Add,
                Rc::new(ExpressionNode {
                    expression: Expression::Infix(
                        Rc::new(ExpressionNode {
                            expression: Expression::Integer(10),
                            typed: AzulaType::Int,
                            span: Span { start: 10, end: 12 },
                        }),
                        Operator::Mul,
                        Rc::new(ExpressionNode {
                            expression: Expression::Integer(5),
                            typed: AzulaType::Int,
                            span: Span { start: 15, end: 16 },
                        })
                    ),
                    typed: AzulaType::Infer,
                    span: Span { start: 10, end: 16 },
                })
            )
        );

        // Brackets
        let input = "(123 + 3) * 10";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap().expression;
        assert_eq!(
            expression,
            Expression::Infix(
                Rc::new(ExpressionNode {
                    expression: Expression::Infix(
                        Rc::new(ExpressionNode {
                            expression: Expression::Integer(123),
                            typed: AzulaType::Int,
                            span: Span { start: 1, end: 4 },
                        }),
                        Operator::Add,
                        Rc::new(ExpressionNode {
                            expression: Expression::Integer(3),
                            typed: AzulaType::Int,
                            span: Span { start: 7, end: 8 },
                        })
                    ),
                    typed: AzulaType::Infer,
                    span: Span { start: 1, end: 8 },
                }),
                Operator::Mul,
                Rc::new(ExpressionNode {
                    expression: Expression::Integer(10),
                    typed: AzulaType::Int,
                    span: Span { start: 12, end: 14 },
                })
            )
        );

        // Comparison
        let input = "x || y";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap().expression;
        assert_eq!(
            expression,
            Expression::Infix(
                Rc::new(ExpressionNode {
                    expression: Expression::Identifier("x".to_string()),
                    typed: AzulaType::Infer,
                    span: Span { start: 0, end: 1 },
                }),
                Operator::Or,
                Rc::new(ExpressionNode {
                    expression: Expression::Identifier("y".to_string()),
                    typed: AzulaType::Infer,
                    span: Span { start: 5, end: 6 },
                })
            )
        );
    }

    #[test]
    fn test_parse_array() {
        let input = "[1, 2, 3]";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expression.expression,
            Expression::Array(vec![
                ExpressionNode {
                    expression: Expression::Integer(1),
                    typed: AzulaType::Int,
                    span: Span { start: 1, end: 2 }
                },
                ExpressionNode {
                    expression: Expression::Integer(2),
                    typed: AzulaType::Int,
                    span: Span { start: 4, end: 5 }
                },
                ExpressionNode {
                    expression: Expression::Integer(3),
                    typed: AzulaType::Int,
                    span: Span { start: 7, end: 8 }
                }
            ])
        );

        // Empty array
        let input = "[]";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(expression.expression, Expression::Array(vec![]));

        // Array initialiser
        let input = "[0; 5]";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expression.expression,
            Expression::Array(vec![
                ExpressionNode {
                    expression: Expression::Integer(0),
                    typed: AzulaType::Int,
                    span: Span { start: 1, end: 2 }
                },
                ExpressionNode {
                    expression: Expression::Integer(0),
                    typed: AzulaType::Int,
                    span: Span { start: 1, end: 2 }
                },
                ExpressionNode {
                    expression: Expression::Integer(0),
                    typed: AzulaType::Int,
                    span: Span { start: 1, end: 2 }
                },
                ExpressionNode {
                    expression: Expression::Integer(0),
                    typed: AzulaType::Int,
                    span: Span { start: 1, end: 2 }
                },
                ExpressionNode {
                    expression: Expression::Integer(0),
                    typed: AzulaType::Int,
                    span: Span { start: 1, end: 2 }
                }
            ])
        );
    }

    #[test]
    fn test_parse_function_call() {
        let input = "test()";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert_eq!(
            expression.expression,
            Expression::FunctionCall {
                function: Rc::new(ExpressionNode {
                    expression: Expression::Identifier("test".to_string()),
                    typed: AzulaType::Infer,
                    span: Span { start: 0, end: 4 },
                }),
                args: vec![],
            }
        );

        let input = "test(5, \"test\")";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        println!("{:?}", parser.errors);
        assert!(parser.errors.is_empty());
        assert_eq!(
            expression.expression,
            Expression::FunctionCall {
                function: Rc::new(ExpressionNode {
                    expression: Expression::Identifier("test".to_string()),
                    typed: AzulaType::Infer,
                    span: Span { start: 0, end: 4 },
                }),
                args: vec![
                    ExpressionNode {
                        expression: Expression::Integer(5),
                        typed: AzulaType::Int,
                        span: Span { start: 5, end: 6 }
                    },
                    ExpressionNode {
                        expression: Expression::String("test".to_string()),
                        typed: AzulaType::Str,
                        span: Span { start: 8, end: 14 }
                    }
                ],
            }
        );
    }

    #[test]
    fn test_string_transform() {
        let test = "\\\\ \\n test \\t";
        let result = string_transform(test).unwrap();
        assert_eq!(result, "\\ \n test \t");

        let test = "\\d";
        let result = string_transform(test);
        assert!(result.is_err());
    }

    #[test]
    fn test_not() {
        let input = "!true";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(parser.errors.is_empty());
        assert_eq!(
            expression.expression,
            Expression::Not(Rc::new(ExpressionNode {
                expression: Expression::Boolean(true),
                typed: AzulaType::Bool,
                span: Span { start: 1, end: 5 }
            }))
        );
    }

    #[test]
    fn test_pointer() {
        let input = "&my_var";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(parser.errors.is_empty());
        assert_eq!(
            expression.expression,
            Expression::Pointer(Rc::new(ExpressionNode {
                expression: Expression::Identifier("my_var".to_string()),
                typed: AzulaType::Infer,
                span: Span { start: 1, end: 7 }
            }))
        );
    }

    #[test]
    fn test_float() {
        let input = "5.5";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(parser.errors.is_empty());
        assert_eq!(expression.expression, Expression::Float(5.5));

        let input = "3.14523";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(parser.errors.is_empty());
        assert_eq!(expression.expression, Expression::Float(3.14523));

        let input = "5.2 * 15.2";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expression = parser.parse_expression(LOWEST, true).unwrap();
        assert!(parser.errors.is_empty());
        assert_eq!(
            expression.expression,
            Expression::Infix(
                Rc::new(ExpressionNode {
                    expression: Expression::Float(5.2),
                    typed: AzulaType::Float,
                    span: Span { start: 0, end: 3 }
                }),
                Operator::Mul,
                Rc::new(ExpressionNode {
                    expression: Expression::Float(15.2),
                    typed: AzulaType::Float,
                    span: Span { start: 6, end: 10 }
                })
            )
        );
    }

    #[test]
    fn test_parse_enum() {
        let input = "enum Color { Red; Green; Blue; }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        if let Statement::Root(body) = parser.parse() {
            assert!(parser.errors.is_empty());
            if let Statement::Enum { name, variants, .. } = &body[0] {
                assert_eq!(*name, "Color");
                assert_eq!(*variants, vec!["Red", "Green", "Blue"]);
            } else {
                panic!("expected Enum statement");
            }
        }
    }

    #[test]
    fn test_parse_match() {
        let input = "match c { Color::Red => 1, Color::Green => 2, _ => 0, }";
        let lexer: Lexer = input.into();
        let mut parser = Parser::new(input, lexer);

        let expr = parser.parse_expression(LOWEST, false).unwrap();
        assert!(parser.errors.is_empty());

        if let Expression::Match(scrutinee, arms) = expr.expression {
            assert_eq!(
                scrutinee.expression,
                Expression::Identifier("c".to_string())
            );
            assert_eq!(arms.len(), 3);
            assert_eq!(arms[0].0, MatchPattern::Variant("Color", "Red"));
            assert_eq!(arms[1].0, MatchPattern::Variant("Color", "Green"));
            assert_eq!(arms[2].0, MatchPattern::Wildcard);
            assert_eq!(arms[0].1.expression, Expression::Integer(1));
            assert_eq!(arms[1].1.expression, Expression::Integer(2));
            assert_eq!(arms[2].1.expression, Expression::Integer(0));
        } else {
            panic!("expected Match expression");
        }
    }
}
