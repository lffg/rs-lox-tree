use std::{iter::Peekable, mem};

use crate::{
    ast::{
        expr::{self, Expr, ExprKind},
        stmt::{self, Stmt, StmtKind},
    },
    parser::{error::ParseError, options::ParserOptions, scanner::Scanner},
    span::Span,
    token::{Token, TokenKind},
};

pub mod error;
pub mod options;
pub mod scanner;

type PResult<T> = Result<T, ParseError>;

pub type ParserOutcome = (Vec<Stmt>, Vec<ParseError>);

pub struct Parser<'src> {
    scanner: Peekable<Scanner<'src>>,
    current_token: Token,
    prev_token: Token,
    diagnostics: Vec<ParseError>,
    pub options: ParserOptions,
}

// The parser implementation.
//
// # Grammar:
//
// -----------------------------------------------------------------------------
//
// program     ::= decl* EOF ;
//
// decl        ::= var_decl
//               | stmt ;
//
// var_decl    ::= "var" IDENTIFIER ( "=" expr )? ";" ;
//
// stmt        ::= if_stmt
//               | print_stmt
//               | block_stmt
//               | expr_stmt ;
//
// if_stmt     ::= "if" "(" expr ")" statement ( "else" statement )? ;
// print_stmt  ::= "print" expr ";" ;
// block_stmt  ::= "{" declaration* "}" ;
// expr_stmt   ::= expr ";" ;
//
// expr        ::= assignment ;
// assignment  ::= IDENTIFIER "=" expr
//               | logic_or ;
// logic_or    ::= logic_and ( "or" logic_and )* ;
// logic_and   ::= equality ( "and" equality )* ;
// equality    ::= comparison ( ( "==" | "!=" ) comparison )* ;
// comparison  ::= term ( ( ">" | ">=" | "<" | "<=" ) term )* ;
// term        ::= factor ( ( "+" | "-" ) factor )* ;
// factor      ::= unary ( ( "*" | "/" ) unary )* ;
// unary       ::= ( "show" | "typeof" | "!" | "-" ) unary
//               | primary ;
// primary     ::= IDENTIFIER
//               | NUMBER | STRING
//               | "true" | "false"
//               | "nil"
//               | "(" expr ")" ;
//
// -----------------------------------------------------------------------------
//
// Each production has a correspondent method in the following implementation.
impl Parser<'_> {
    pub fn parse(mut self) -> ParserOutcome {
        (self.parse_program(), self.diagnostics)
    }

    fn parse_program(&mut self) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        while !self.is_at_end() {
            // TODO: Maybe synchronize at `parse_decl`.
            stmts.push(self.parse_decl().unwrap_or_else(|error| {
                self.diagnostics.push(error);
                self.synchronize();
                let hi = self.current_token.span.hi;
                Stmt {
                    kind: stmt::Dummy().into(),
                    span: Span::new(hi, hi),
                }
            }));
        }
        stmts
    }

    //
    // Declarations
    //

    fn parse_decl(&mut self) -> PResult<Stmt> {
        match self.current_token.kind {
            TokenKind::Var => self.parse_var_decl(),
            _ => self.parse_stmt(),
        }
    }

    fn parse_var_decl(&mut self) -> PResult<Stmt> {
        use TokenKind::*;
        let var_span = self.consume(Var, "")?.span;
        if let Identifier(ref name) = self.current_token.kind {
            let name = name.clone();
            let name_span = self.advance().span;

            let mut init = None;
            if self.take(Equal) {
                init = Some(self.parse_expr()?);
            }

            let semicolon_span = self
                .consume(Semicolon, "Expected `;` after variable declaration")?
                .span;

            return Ok(Stmt {
                kind: StmtKind::from(stmt::Var {
                    name,
                    name_span,
                    init,
                }),
                span: var_span.to(semicolon_span),
            });
        }

        Err(self.unexpected("Expected variable name", Some(Identifier("<ident>".into()))))
    }

    //
    // Statements
    //

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        match self.current_token.kind {
            TokenKind::If => self.parse_if_stmt(),
            TokenKind::Print => self.parse_print_stmt(),
            TokenKind::LeftBrace => {
                let (stmts, span) = self.parse_block()?;
                let kind = stmt::Block { stmts }.into();
                Ok(Stmt { kind, span })
            }
            _ => self.parse_expr_stmt(),
        }
    }

    fn parse_if_stmt(&mut self) -> PResult<Stmt> {
        use TokenKind::*;
        let if_token_span = self.consume(If, "")?.span;

        self.consume(LeftParen, "Expect parenthesized expression after `if`.")?;
        let cond = self.parse_expr()?;
        self.consume(RightParen, "Must close parentheses after `if`.")?;

        let then_branch = self.parse_stmt()?;
        let mut else_branch = None;
        if self.take(Else) {
            else_branch = Some(self.parse_stmt()?);
        }

        Ok(Stmt {
            span: if_token_span.to(else_branch
                .as_ref()
                .map(|it| it.span)
                .unwrap_or(then_branch.span)),
            kind: StmtKind::from(stmt::If {
                cond,
                then_branch: then_branch.into(),
                else_branch: else_branch.map(|it| it.into()),
            }),
        })
    }

    fn parse_print_stmt(&mut self) -> PResult<Stmt> {
        let print_token_span = self.consume(TokenKind::Print, "")?.span;
        let expr = self.parse_expr()?;
        let semicolon_span = self
            .consume(TokenKind::Semicolon, "Expected `;` after value")?
            .span;
        Ok(Stmt {
            span: print_token_span.to(semicolon_span),
            kind: stmt::Print { expr, debug: false }.into(),
        })
    }

    fn parse_block(&mut self) -> PResult<(Vec<Stmt>, Span)> {
        let left_brace_span = self.consume(TokenKind::LeftBrace, "")?.span;
        let mut stmts = Vec::new();
        while !self.is(&TokenKind::RightBrace) && !self.is_at_end() {
            stmts.push(self.parse_decl()?);
        }
        let right_brace_span = self
            .consume(TokenKind::RightBrace, "Expected block to be closed")?
            .span;
        Ok((stmts, left_brace_span.to(right_brace_span)))
    }

    fn parse_expr_stmt(&mut self) -> PResult<Stmt> {
        let expr = self.parse_expr()?;

        // If the parser is running in the REPL mode and the next token is of kind `Eof`, it will
        // emit a `Print` statement in order to show the given expression's value.
        if self.options.repl_mode && self.is_at_end() {
            return Ok(Stmt {
                span: expr.span,
                kind: stmt::Print { expr, debug: true }.into(),
            });
        }

        let semicolon_span = self
            .consume(TokenKind::Semicolon, "Expected `;` after expression")?
            .span;
        Ok(Stmt {
            span: expr.span.to(semicolon_span),
            kind: stmt::Expr { expr }.into(),
        })
    }

    //
    // Expressions
    //

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_assignment()
    }

    fn parse_assignment(&mut self) -> PResult<Expr> {
        // The parser does not yet know if `left` should be used as an expression (i.e. an rvalue)
        // or as an "assignment target" (i.e. an lvalue).
        let left = self.parse_or()?;

        if self.take(TokenKind::Equal) {
            // Since assignments are right associative, we use right recursion to parse its value.
            // The right-most assignment value should be evaluated first (down in the parse tree),
            // so it should be parsed last. This semantic can be coded with this kind of recursion.
            let value = self.parse_assignment()?;

            // Now the parser knows that `left` must be an lvalue.
            if let ExprKind::Var(expr::Var { name }) = left.kind {
                return Ok(Expr {
                    span: left.span.to(value.span),
                    kind: ExprKind::from(expr::Assignment {
                        name,
                        name_span: left.span,
                        value: value.into(),
                    }),
                });
            }

            return Err(ParseError::Error {
                message: "Invalid assignment target.".into(),
                span: left.span,
            });
        }

        Ok(left)
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        bin_expr!(
            self,
            parse_as = Logical,
            token_kinds = Or,
            next_production = parse_and
        )
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        bin_expr!(
            self,
            parse_as = Logical,
            token_kinds = And,
            next_production = parse_equality
        )
    }

    fn parse_equality(&mut self) -> PResult<Expr> {
        bin_expr!(
            self,
            parse_as = Binary,
            token_kinds = EqualEqual | BangEqual,
            next_production = parse_comparison
        )
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        bin_expr!(
            self,
            parse_as = Binary,
            token_kinds = Greater | GreaterEqual | Less | LessEqual,
            next_production = parse_term
        )
    }

    fn parse_term(&mut self) -> PResult<Expr> {
        bin_expr!(
            self,
            parse_as = Binary,
            token_kinds = Plus | Minus,
            next_production = parse_factor
        )
    }

    fn parse_factor(&mut self) -> PResult<Expr> {
        bin_expr!(
            self,
            parse_as = Binary,
            token_kinds = Star | Slash,
            next_production = parse_unary
        )
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        use TokenKind::*;
        if let Bang | Minus | Typeof | Show = self.current_token.kind {
            let operator = self.advance().clone();
            let operand = self.parse_unary()?;
            return Ok(Expr {
                span: operator.span.to(operand.span),
                kind: ExprKind::from(expr::Unary {
                    operator,
                    operand: operand.into(),
                }),
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        use TokenKind::*;
        match self.current_token.kind {
            String(_) | Number(_) | True | False | Nil => {
                let token = self.advance();
                Ok(Expr {
                    kind: expr::Lit::from(token.clone()).into(),
                    span: token.span,
                })
            }
            Identifier(ref name) => Ok(Expr {
                kind: expr::Var { name: name.clone() }.into(),
                span: self.advance().span,
            }),
            LeftParen => {
                let left_paren_span = self.advance().span;
                let expr = self.parse_expr()?.into();
                let right_paren_span = self
                    .consume(RightParen, "Expected group to be closed")?
                    .span;
                Ok(Expr {
                    kind: expr::Group { expr }.into(),
                    span: left_paren_span.to(right_paren_span),
                })
            }
            _ => Err(self.unexpected("Expected any expression", None)),
        }
    }
}

// The parser helper methods.
impl<'src> Parser<'src> {
    /// Creates a new parser.
    pub fn new(src: &'src str) -> Self {
        let mut parser = Self {
            scanner: Scanner::new(src).peekable(),
            current_token: Token::dummy(),
            prev_token: Token::dummy(),
            diagnostics: Vec::new(),
            options: Default::default(),
        };
        parser.advance(); // The first advancement.
        parser
    }

    /// Advances the parser and returns a reference to the `prev_token` field.
    fn advance(&mut self) -> &Token {
        let next = loop {
            let maybe_next = self.scanner.next().expect("Cannot advance past Eof.");
            // Report and ignore tokens with the `Error` kind:
            if let TokenKind::Error(message) = maybe_next.kind {
                self.diagnostics.push(ParseError::ScannerError {
                    span: maybe_next.span,
                    message,
                });
                continue;
            }
            // Handle other common ignored kinds:
            if let TokenKind::Comment(_) | TokenKind::Whitespace(_) = maybe_next.kind {
                continue;
            }
            break maybe_next;
        };
        self.prev_token = mem::replace(&mut self.current_token, next);
        &self.prev_token
    }

    /// Checks if the current token matches the kind of the given one.
    #[inline]
    fn is(&mut self, expected: &TokenKind) -> bool {
        mem::discriminant(&self.current_token.kind) == mem::discriminant(expected)
    }

    /// Checks if the current token matches the kind of the given one. In such case advances and
    /// returns true. Otherwise returns false.
    fn take(&mut self, expected: TokenKind) -> bool {
        let res = self.is(&expected);
        if res {
            self.advance();
        }
        res
    }

    /// Checks if the current token matches the kind of the given one. In such case advances and
    /// returns `Ok(_)` with the consumed token. Otherwise returns an expectation error with the
    /// provided message.
    fn consume(&mut self, expected: TokenKind, msg: impl Into<String>) -> PResult<&Token> {
        if self.is(&expected) {
            Ok(self.advance())
        } else {
            Err(self.unexpected(msg, Some(expected)))
        }
    }

    /// Returns an `ParseError::UnexpectedToken`.
    #[inline(always)]
    fn unexpected(&self, message: impl Into<String>, expected: Option<TokenKind>) -> ParseError {
        let mut message = message.into();
        if message.is_empty() {
            message = "The spanned token is not expected. This is a compiler bug.".into();
        }
        ParseError::UnexpectedToken {
            message,
            expected,
            offending: self.current_token.clone(),
        }
    }

    /// Synchronizes the parser state with the current token.
    /// A synchronization is needed in order to match the parser state to the current token.
    ///
    /// When an error is encountered in a `parse_*` method, a `ParseError` is returned. These kind
    /// of errors are forwarded using the `?` operator, which, in practice, unwinds the parser
    /// stack frame. Hence the question mark operator should not be used in synchronization points.
    /// Such synchronization points call this method.
    ///
    /// The synchronization process discards all tokens until it reaches a grammar rule which marks
    /// a synchronization point.
    ///
    /// In this implementation, synchronizations are manually performed in statement boundaries:
    ///   * If the previous token is a semicolon, the parser is *probably* (exceptions exists, such
    ///     as a semicolon in a for loop) starting a new statement.
    ///   * If the next token marks the start of a new statement.
    ///
    /// Before synchronize one must not forget to emit the raised parse error.
    fn synchronize(&mut self) {
        // If the end is already reached any further advancements are needless.
        if self.is_at_end() {
            return;
        }

        self.advance();
        use TokenKind::*;
        while !{
            let curr = &self.current_token.kind;
            let prev = &self.prev_token.kind;

            self.is_at_end()
                || matches!(prev, Semicolon)
                || matches!(curr, Class | For | Fun | If | Print | Return | Var | While)
        } {
            self.advance();
        }
    }

    /// Checks if the parser has finished.
    #[inline]
    fn is_at_end(&self) -> bool {
        self.current_token.kind == TokenKind::Eof
    }
}

/// Parses a binary expression.
macro_rules! bin_expr {
    ($self:expr, parse_as = $ast_kind:ident, token_kinds = $( $kind:ident )|+, next_production = $next:ident) => {{
        let mut expr = $self.$next()?;
        while let $( TokenKind::$kind )|+ = $self.current_token.kind {
            let operator = $self.advance().clone();
            let right = $self.$next()?;
            expr = Expr {
                span: expr.span.to(right.span),
                kind: ExprKind::from(expr::$ast_kind {
                    left: expr.into(),
                    operator,
                    right: right.into(),
                }),
            };
        }
        Ok(expr)
    }};
}
use bin_expr;
