use bitflags::bitflags;

use ast::Mod;
use ruff_python_ast::{self as ast, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::lexer::lex;
use crate::parser::progress::ParserProgress;
use crate::{
    lexer::{LexResult, Spanned},
    token_set::TokenSet,
    token_source::TokenSource,
    Mode, ParseError, ParseErrorType, Tok, TokenKind,
};

mod expression;
mod helpers;
mod pattern;
mod progress;
mod statement;
#[cfg(test)]
mod tests;

pub(crate) fn parse_tokens(
    tokens: Vec<LexResult>,
    source: &str,
    mode: Mode,
) -> Result<Mod, ParseError> {
    let program = Parser::new(source, mode, TokenSource::new(tokens)).parse_program();
    if program.parse_errors.is_empty() {
        Ok(program.ast)
    } else {
        Err(program.parse_errors.into_iter().next().unwrap())
    }
}

#[derive(Debug)]
pub struct Program {
    pub ast: ast::Mod,
    pub parse_errors: Vec<ParseError>,
}

impl Program {
    pub fn parse_str(source: &str, mode: Mode) -> Program {
        let tokens = lex(source, mode);
        Self::parse_tokens(source, tokens.collect(), mode)
    }

    pub fn parse_tokens(source: &str, tokens: Vec<LexResult>, mode: Mode) -> Program {
        Parser::new(source, mode, TokenSource::new(tokens)).parse_program()
    }
}

pub(crate) struct Parser<'src> {
    source: &'src str,
    tokens: TokenSource,

    /// Stores all the syntax errors found during the parsing.
    errors: Vec<ParseError>,

    /// This tracks the current expression or statement being parsed. For example,
    /// if we're parsing a tuple expression, e.g. `(1, 2)`, `ctx` has the value
    /// `ParserCtxFlags::TUPLE_EXPR`.
    ///
    /// The `ctx` is also used to create custom error messages and forbid certain
    /// expressions or statements of being parsed. The `ctx` should be empty after
    /// an expression or statement is done parsing.
    ctx: ParserCtxFlags,

    /// During the parsing of expression or statement, multiple `ctx`s can be created.
    /// `ctx_stack` stores the previous `ctx`s that were created during the parsing. For example,
    /// when parsing a tuple expression, e.g. `(1, 2, 3)`, two [`ParserCtxFlags`] will be
    /// created `ParserCtxFlags::PARENTHESIZED_EXPR` and `ParserCtxFlags::TUPLE_EXPR`.
    ///
    /// When parsing a tuple the first context created is `ParserCtxFlags::PARENTHESIZED_EXPR`.
    /// Afterwards, the `ParserCtxFlags::TUPLE_EXPR` is created and `ParserCtxFlags::PARENTHESIZED_EXPR`
    /// is pushed onto the `ctx_stack`.
    /// `ParserCtxFlags::PARENTHESIZED_EXPR` is removed from the stack and set to be the current `ctx`,
    /// after we parsed all elements in the tuple.
    ///
    /// The end of the vector is the top of the stack.
    ctx_stack: Vec<ParserCtxFlags>,

    /// Stores the last `ctx` of an expression or statement that was parsed.
    last_ctx: ParserCtxFlags,

    /// Specify the mode in which the code will be parsed.
    mode: Mode,

    current: Spanned,

    /// The end of the last processed. Used to determine a node's end.
    last_token_end: TextSize,

    /// The range of the tokens to parse.
    ///
    /// The range is equal to [0; source.len()) when parsing an entire file.
    /// The range can be different when parsing only a part of a file using the `lex_starts_at` and `parse_expression_starts_at` APIs
    /// in which case the the range is equal to [offset; subrange.len()).
    tokens_range: TextRange,
}

const NEWLINE_EOF_SET: TokenSet = TokenSet::new(&[TokenKind::Newline, TokenKind::EndOfFile]);
const LITERAL_SET: TokenSet = TokenSet::new(&[
    TokenKind::Name,
    TokenKind::Int,
    TokenKind::Float,
    TokenKind::Complex,
    TokenKind::Plus,
    TokenKind::String,
    TokenKind::Ellipsis,
    TokenKind::True,
    TokenKind::False,
    TokenKind::None,
]);
/// Tokens that are usually an expression or the start of one.
const EXPR_SET: TokenSet = TokenSet::new(&[
    TokenKind::Minus,
    TokenKind::Tilde,
    TokenKind::Star,
    TokenKind::DoubleStar,
    TokenKind::Vbar,
    TokenKind::Lpar,
    TokenKind::Lbrace,
    TokenKind::Lsqb,
    TokenKind::Lambda,
    TokenKind::Await,
    TokenKind::Not,
    TokenKind::Yield,
    TokenKind::FStringStart,
])
.union(LITERAL_SET);

impl<'src> Parser<'src> {
    pub(crate) fn new(source: &'src str, mode: Mode, mut tokens: TokenSource) -> Parser<'src> {
        let tokens_range = TextRange::new(
            tokens.position().unwrap_or_default(),
            tokens.end().unwrap_or_default(),
        );

        let current = tokens
            .next()
            .unwrap_or_else(|| (Tok::EndOfFile, TextRange::empty(tokens_range.end())));

        Parser {
            mode,
            source,
            errors: Vec::new(),
            ctx_stack: Vec::new(),
            ctx: ParserCtxFlags::empty(),
            last_ctx: ParserCtxFlags::empty(),
            tokens,
            last_token_end: tokens_range.start(),
            current,
            tokens_range,
        }
    }

    pub(crate) fn parse_program(mut self) -> Program {
        let mut body = vec![];

        let ast = if self.mode == Mode::Expression {
            let start = self.node_start();
            let parsed_expr = self.parse_expression();
            let mut progress = ParserProgress::default();

            // TODO: How should error recovery work here? Just truncate after the expression?
            loop {
                progress.assert_progressing(&self);
                if !self.eat(TokenKind::Newline) {
                    break;
                }
            }
            self.bump(TokenKind::EndOfFile);

            ast::Mod::Expression(ast::ModExpression {
                body: Box::new(parsed_expr.expr),
                range: self.node_range(start),
            })
        } else {
            let start = self.tokens_range.start();
            let mut progress = ParserProgress::default();
            while !self.at(TokenKind::EndOfFile) {
                progress.assert_progressing(&self);
                if self.at(TokenKind::Indent) {
                    self.handle_unexpected_indentation(&mut body, "unexpected indentation");
                    continue;
                }

                if self.is_at_stmt() {
                    body.push(self.parse_statement());
                } else {
                    // SKip over the unexpected token.
                    self.next_token();
                }
            }

            self.bump(TokenKind::EndOfFile);

            ast::Mod::Module(ast::ModModule {
                body,
                // If the `source` only contains comments or empty spaces, return
                // an empty range.
                // FIXME(micha): The module should always enclose the entire file
                range: self.node_range(start),
            })
        };

        Program {
            ast,
            parse_errors: self.finish(),
        }
    }

    fn finish(self) -> Vec<ParseError> {
        // After parsing, the `ctx` and `ctx_stack` should be empty.
        // If it's not, you probably forgot to call `clear_ctx` somewhere.
        assert_eq!(self.ctx, ParserCtxFlags::empty());
        assert_eq!(&self.ctx_stack, &[]);
        assert_eq!(
            self.current_kind(),
            TokenKind::EndOfFile,
            "Parser should be at the end of the file."
        );

        // TODO consider re-integrating lexical error handling into the parser?
        let parse_errors = self.errors;
        let lex_errors = self.tokens.finish();

        // Fast path for when there are no lex errors.
        // There's no fast path for when there are no parse errors because a lex error
        // always results in a parse error.
        if lex_errors.is_empty() {
            return parse_errors;
        }

        let mut merged = Vec::with_capacity(parse_errors.len().saturating_add(lex_errors.len()));

        let mut parse_errors = parse_errors.into_iter().peekable();
        let mut lex_errors = lex_errors.into_iter().peekable();

        while let (Some(parse_error), Some(lex_error)) = (parse_errors.peek(), lex_errors.peek()) {
            if parse_error.location.start() < lex_error.location.start() {
                merged.push(parse_errors.next().unwrap());
            } else {
                merged.push(ParseError::from(lex_errors.next().unwrap()));
            }
        }

        merged.extend(parse_errors);
        merged.extend(lex_errors.map(ParseError::from));

        merged
    }

    #[inline]
    fn set_ctx(&mut self, ctx: ParserCtxFlags) {
        self.ctx_stack.push(self.ctx);
        self.ctx = ctx;
    }

    #[inline]
    fn clear_ctx(&mut self, ctx: ParserCtxFlags) {
        assert_eq!(self.ctx, ctx);
        self.last_ctx = ctx;
        if let Some(top) = self.ctx_stack.pop() {
            self.ctx = top;
        }
    }

    #[inline]
    fn has_ctx(&self, ctx: ParserCtxFlags) -> bool {
        self.ctx.intersects(ctx)
    }

    /// Returns the start position for a node that starts at the current token.
    fn node_start(&self) -> TextSize {
        self.current_range().start()
    }

    /// FIXME(micha): This panics when the parsing doesn't consume any tokens. because `start` points to the start of the current
    /// token but `last_token_end` points to the end of the previous token. Not sure how to fix this best in a reliable manner.
    /// One solution could be to return a `Marker` from `node_start` that stores the count of the start token and the start offset.
    /// `node_range` could then use the count to see if the parser is still positioned at the same token in which case
    /// it returns `TextRange::empty(self.current_range().start())`.
    ///
    /// The downside of this is that it becomes necessary to store the current token index on the parser, which requires a
    /// write operation during each `next` call which is unfortunate.
    ///
    /// There's also the question if it works if the parser consumed a `Dedent` or `Newline` token but `last_token_end` still
    /// points to the previous token because we don't update the end position for these tokens.
    fn node_range(&self, start: TextSize) -> TextRange {
        TextRange::new(start, self.last_token_end)
    }

    fn missing_node_range(&self) -> TextRange {
        TextRange::empty(self.last_token_end)
    }

    /// Moves the parser to the next token. Returns the old current token as an owned value.
    /// FIXME(micha): Using `next_token` is almost always incorrect if there's a case where the current token is not of the expected type.
    fn next_token(&mut self) -> Spanned {
        let next = self
            .tokens
            .next()
            .unwrap_or_else(|| (Tok::EndOfFile, TextRange::empty(self.tokens_range.end())));

        let current = std::mem::replace(&mut self.current, next);

        if !matches!(
            current.0,
            // TODO explore including everything up to the dedent as part of the body.
            Tok::Dedent
            // Don't include newlines in the body
            | Tok::Newline
            // TODO(micha): Including the semi feels more correct but it isn't compatible with lalrpop and breaks the
            // formatters semicolon detection. Exclude it for now
            | Tok::Semi
        ) {
            self.last_token_end = current.1.end();
        }

        current
    }

    fn peek_nth(&self, offset: usize) -> TokenKind {
        if offset == 0 {
            self.current_kind()
        } else {
            self.tokens
                .peek_nth(offset - 1)
                .map_or(TokenKind::EndOfFile, |spanned| spanned.0)
        }
    }

    #[inline]
    fn current_token(&self) -> (TokenKind, TextRange) {
        (self.current_kind(), self.current_range())
    }

    #[inline]
    fn current_kind(&self) -> TokenKind {
        // TODO: Converting the token kind over and over again can be expensive.
        TokenKind::from_token(&self.current.0)
    }

    #[inline]
    fn current_range(&self) -> TextRange {
        self.current.1
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if !self.at(kind) {
            return false;
        }

        self.next_token();
        true
    }

    /// Bumps the current token assuming it is of the given kind.
    ///
    /// # Panics
    /// If the current token is not of the given kind.
    ///
    /// # Returns
    /// The current token
    fn bump(&mut self, kind: TokenKind) -> (Tok, TextRange) {
        assert_eq!(self.current_kind(), kind);

        self.next_token()
    }

    fn expect(&mut self, expected: TokenKind) -> bool {
        if self.eat(expected) {
            return true;
        }

        let (found, range) = self.current_token();
        self.add_error(ParseErrorType::ExpectedToken { found, expected }, range);
        false
    }

    fn add_error<T>(&mut self, error: ParseErrorType, ranged: T)
    where
        T: Ranged,
    {
        fn inner(errors: &mut Vec<ParseError>, error: ParseErrorType, range: TextRange) {
            // Avoid flagging multiple errors at the same location
            let is_same_location = errors
                .last()
                .is_some_and(|last| last.location.start() == range.start());

            if !is_same_location {
                errors.push(ParseError {
                    error,
                    location: range,
                });
            }
        }

        inner(&mut self.errors, error, ranged.range());
    }

    /// Skip tokens until [`TokenSet`]. Returns the range of the skipped tokens.
    fn skip_until(&mut self, token_set: TokenSet) {
        let mut progress = ParserProgress::default();
        while !self.at_ts(token_set) {
            progress.assert_progressing(self);
            self.next_token();
        }
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.current_kind() == kind
    }

    fn at_ts(&self, ts: TokenSet) -> bool {
        ts.contains(self.current_kind())
    }

    fn src_text<T>(&self, ranged: T) -> &'src str
    where
        T: Ranged,
    {
        let range = ranged.range();
        // `ranged` uses absolute ranges to the source text of an entire file.
        // Fix the source by subtracting the start offset when parsing only a part of a file (when parsing the tokens from `lex_starts_at`).
        &self.source[range - self.tokens_range.start()]
    }

    /// Parses elements enclosed within a delimiter pair, such as parentheses, brackets,
    /// or braces.
    fn parse_delimited(
        &mut self,
        allow_trailing_delim: bool,
        opening: TokenKind,
        delim: TokenKind,
        closing: TokenKind,
        func: impl FnMut(&mut Parser<'src>),
    ) {
        self.bump(opening);

        self.parse_separated(allow_trailing_delim, delim, [closing].as_slice(), func);

        self.expect(closing);
    }

    /// Parses a sequence of elements separated by a delimiter. This function stops
    /// parsing upon encountering any of the tokens in `ending_set`, if it doesn't
    /// encounter the tokens in `ending_set` it stops parsing when seeing the `EOF`
    /// or `Newline` token.
    fn parse_separated(
        &mut self,
        allow_trailing_delim: bool,
        delim: TokenKind,
        ending_set: impl Into<TokenSet>,
        mut func: impl FnMut(&mut Parser<'src>),
    ) {
        let ending_set = NEWLINE_EOF_SET.union(ending_set.into());
        let mut progress = ParserProgress::default();

        while !self.at_ts(ending_set) {
            progress.assert_progressing(self);
            func(self);

            // exit the loop if a trailing `delim` is not allowed
            if !allow_trailing_delim && ending_set.contains(self.peek_nth(1)) {
                break;
            }

            if !self.eat(delim) {
                if self.at_expr() {
                    self.expect(delim);
                } else {
                    break;
                }
            }
        }
    }

    fn is_current_token_postfix(&self) -> bool {
        matches!(
            self.current_kind(),
            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Dot | TokenKind::Async | TokenKind::For
        )
    }

    fn handle_unexpected_indentation(&mut self, stmts: &mut Vec<Stmt>, error_msg: &str) {
        self.bump(TokenKind::Indent);

        self.add_error(
            ParseErrorType::OtherError(error_msg.to_string()),
            self.current_range(),
        );

        let mut progress = ParserProgress::default();

        while !self.at(TokenKind::Dedent) && !self.at(TokenKind::EndOfFile) {
            progress.assert_progressing(self);

            let stmt = self.parse_statement();
            stmts.push(stmt);
        }

        assert!(self.eat(TokenKind::Dedent));
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SequenceMatchPatternParentheses {
    Tuple,
    List,
}

impl SequenceMatchPatternParentheses {
    fn closing_kind(self) -> TokenKind {
        match self {
            SequenceMatchPatternParentheses::Tuple => TokenKind::Rpar,
            SequenceMatchPatternParentheses::List => TokenKind::Rsqb,
        }
    }

    const fn is_list(self) -> bool {
        matches!(self, SequenceMatchPatternParentheses::List)
    }
}

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    struct ParserCtxFlags: u8 {
        const PARENTHESIZED_EXPR = 1 << 0;

        // NOTE: `ARGUMENTS` can be removed once the heuristic in `parse_with_items`
        // is improved.
        const ARGUMENTS = 1 << 1;
        const FOR_TARGET = 1 << 2;
    }
}

#[derive(PartialEq, Copy, Clone)]
enum FunctionKind {
    Lambda,
    FunctionDef,
}
