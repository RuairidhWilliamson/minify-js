use std::collections::{HashMap, HashSet};
use std::ops::Index;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use lazy_static::lazy_static;
use memchr::{memchr, memchr2, memchr3};

use crate::char::{
    CharFilter, DIGIT, DIGIT_BIN, DIGIT_HEX, DIGIT_OCT, ID_CONTINUE,
    ID_CONTINUE_OR_PARENTHESIS_CLOSE_OR_BRACKET_CLOSE, ID_START_CHARSTR, WHITESPACE,
};
use crate::error::{SyntaxError, SyntaxErrorType, TsResult};
use crate::source::{Source, SourceRange};
use crate::token::{Token, TokenType};

#[cfg(test)]
mod tests;

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum LexMode {
    SlashIsRegex,
    Standard,
}

#[derive(Copy, Clone)]
pub struct LexerCheckpoint {
    next: usize,
}

#[derive(Copy, Clone)]
struct Match {
    len: usize,
}

impl Match {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn prefix(&self, n: usize) -> Match {
        debug_assert!(n <= self.len);
        Match { len: n }
    }
}

#[derive(Copy, Clone)]
struct AhoCorasickMatch {
    id: usize,
    mat: Match,
}

pub struct Lexer {
    source: Source,
    next: usize,
}

impl Lexer {
    pub fn new(code: Vec<u8>) -> Lexer {
        Lexer {
            source: Source::new(code),
            next: 0,
        }
    }

    fn end(&self) -> usize {
        self.source.code().len()
    }

    fn remaining(&self) -> usize {
        self.end() - self.next
    }

    pub fn source_range(&self) -> SourceRange {
        SourceRange {
            source: self.source.clone(),
            start: 0,
            end: self.end(),
        }
    }

    fn eof_range(&self) -> SourceRange {
        SourceRange {
            source: self.source.clone(),
            start: self.end(),
            end: self.end(),
        }
    }

    fn error(&self, typ: SyntaxErrorType) -> SyntaxError {
        SyntaxError::new(typ, self.source.clone(), self.next, None)
    }

    fn at_end(&self) -> bool {
        self.next >= self.end()
    }

    fn prev_char(&self) -> u8 {
        // 0xFF is not a valid byte in any valid UTF-8 byte sequence.
        *self
            .source
            .code()
            .get(self.next.wrapping_sub(1))
            .unwrap_or(&b'\xFF')
    }

    fn peek(&self, n: usize) -> TsResult<u8> {
        self.peek_or_eof(n)
            .ok_or_else(|| self.error(SyntaxErrorType::UnexpectedEnd))
    }

    fn peek_or_eof(&self, n: usize) -> Option<u8> {
        self.source.code().get(self.next + n).map(|&c| c)
    }

    fn consume_next_char(&mut self) -> TsResult<(SourceRange, u8)> {
        if self.at_end() {
            Err(self.error(SyntaxErrorType::UnexpectedEnd))
        } else {
            let c = self.source.code()[self.next];
            let r = SourceRange {
                source: self.source.clone(),
                start: self.next,
                end: self.next + 1,
            };
            self.next += 1;
            Ok((r, c))
        }
    }

    pub fn checkpoint(&self) -> LexerCheckpoint {
        LexerCheckpoint { next: self.next }
    }

    pub fn since_checkpoint(&self, checkpoint: LexerCheckpoint) -> SourceRange {
        SourceRange {
            source: self.source.clone(),
            start: checkpoint.next,
            end: self.next,
        }
    }

    pub fn apply_checkpoint(&mut self, checkpoint: LexerCheckpoint) -> () {
        self.next = checkpoint.next;
    }

    fn n(&self, n: usize) -> TsResult<Match> {
        if self.next + n > self.end() {
            return Err(self.error(SyntaxErrorType::UnexpectedEnd));
        };
        Ok(Match { len: n })
    }

    fn if_char(&self, c: u8) -> Match {
        Match {
            len: (!self.at_end() && self.source.code()[self.next] == c) as usize,
        }
    }

    fn through_char(&self, c: u8) -> TsResult<Match> {
        memchr(c, &self.source.code()[self.next..])
            .map(|pos| Match { len: pos + 1 })
            .ok_or_else(|| self.error(SyntaxErrorType::UnexpectedEnd))
    }

    fn while_not_2_chars(&self, a: u8, b: u8) -> Match {
        Match {
            len: memchr2(a, b, &self.source.code()[self.next..]).unwrap_or(self.remaining()),
        }
    }

    fn while_not_3_chars(&self, a: u8, b: u8, c: u8) -> Match {
        Match {
            len: memchr3(a, b, c, &self.source.code()[self.next..]).unwrap_or(self.remaining()),
        }
    }

    fn while_chars(&self, chars: &CharFilter) -> Match {
        let mut len = 0;
        while len < self.remaining() && chars.has(self.source.code()[self.next + len]) {
            len += 1;
        }
        Match { len }
    }

    fn aho_corasick(&self, ac: &AhoCorasick) -> TsResult<AhoCorasickMatch> {
        ac.find(&self.source.code()[self.next..])
            .map(|m| AhoCorasickMatch {
                id: m.pattern(),
                mat: Match { len: m.end() },
            })
            .ok_or_else(|| self.error(SyntaxErrorType::ExpectedNotFound))
    }

    fn range(&self, m: Match) -> SourceRange {
        SourceRange {
            source: self.source.clone(),
            start: self.next,
            end: self.next + m.len,
        }
    }

    fn consume(&mut self, m: Match) -> () {
        self.next += m.len;
    }

    fn skip_expect(&mut self, n: usize) -> () {
        debug_assert!(self.next + n <= self.end());
        self.next += n;
    }
}

impl Index<SourceRange> for Lexer {
    type Output = [u8];

    fn index(&self, index: SourceRange) -> &Self::Output {
        &self.source.code()[index.start..index.end]
    }
}

impl Index<Match> for Lexer {
    type Output = [u8];

    fn index(&self, index: Match) -> &Self::Output {
        &self.source.code()[self.next - index.len..self.next]
    }
}

lazy_static! {
    pub static ref OPERATORS_MAPPING: HashMap<TokenType, &'static [u8]> = {
        let mut map = HashMap::<TokenType, &'static [u8]>::new();
        map.insert(TokenType::Ampersand, b"&");
        map.insert(TokenType::AmpersandAmpersand, b"&&");
        map.insert(TokenType::AmpersandAmpersandEquals, b"&&=");
        map.insert(TokenType::AmpersandEquals, b"&=");
        map.insert(TokenType::Asterisk, b"*");
        map.insert(TokenType::AsteriskAsterisk, b"**");
        map.insert(TokenType::AsteriskAsteriskEquals, b"**=");
        map.insert(TokenType::AsteriskEquals, b"*=");
        map.insert(TokenType::Bar, b"|");
        map.insert(TokenType::BarBar, b"||");
        map.insert(TokenType::BarBarEquals, b"||=");
        map.insert(TokenType::BarEquals, b"|=");
        map.insert(TokenType::BraceClose, b"}");
        map.insert(TokenType::BraceOpen, b"{");
        map.insert(TokenType::BracketClose, b"]");
        map.insert(TokenType::BracketOpen, b"[");
        map.insert(TokenType::Caret, b"^");
        map.insert(TokenType::CaretEquals, b"^=");
        map.insert(TokenType::ChevronLeft, b"<");
        map.insert(TokenType::ChevronLeftChevronLeft, b"<<");
        map.insert(TokenType::ChevronLeftChevronLeftEquals, b"<<=");
        map.insert(TokenType::ChevronLeftEquals, b"<=");
        map.insert(TokenType::ChevronRight, b">");
        map.insert(TokenType::ChevronRightChevronRight, b">>");
        map.insert(TokenType::ChevronRightChevronRightChevronRight, b">>>");
        map.insert(TokenType::ChevronRightChevronRightChevronRightEquals, b">>>=");
        map.insert(TokenType::ChevronRightChevronRightEquals, b">>=");
        map.insert(TokenType::ChevronRightEquals, b">=");
        map.insert(TokenType::Colon, b":");
        map.insert(TokenType::Comma, b",");
        map.insert(TokenType::Dot, b".");
        map.insert(TokenType::DotDotDot, b"...");
        map.insert(TokenType::Equals, b"=");
        map.insert(TokenType::EqualsChevronRight, b"=>");
        map.insert(TokenType::EqualsEquals, b"==");
        map.insert(TokenType::EqualsEqualsEquals, b"===");
        map.insert(TokenType::Exclamation, b"!");
        map.insert(TokenType::ExclamationEquals, b"!=");
        map.insert(TokenType::ExclamationEqualsEquals, b"!==");
        map.insert(TokenType::Hyphen, b"-");
        map.insert(TokenType::HyphenEquals, b"-=");
        map.insert(TokenType::HyphenHyphen, b"--");
        map.insert(TokenType::ParenthesisClose, b")");
        map.insert(TokenType::ParenthesisOpen, b"(");
        map.insert(TokenType::Percent, b"%");
        map.insert(TokenType::PercentEquals, b"%=");
        map.insert(TokenType::Plus, b"+");
        map.insert(TokenType::PlusEquals, b"+=");
        map.insert(TokenType::PlusPlus, b"++");
        map.insert(TokenType::Question, b"?");
        map.insert(TokenType::QuestionDot, b"?.");
        map.insert(TokenType::QuestionQuestion, b"??");
        map.insert(TokenType::Semicolon, b";");
        map.insert(TokenType::Slash, b"/");
        map.insert(TokenType::SlashEquals, b"/=");
        map.insert(TokenType::Tilde, b"~");
        map
    };

    pub static ref KEYWORDS_MAPPING: HashMap<TokenType, &'static [u8]> = {
        let mut map = HashMap::<TokenType, &'static [u8]>::new();
        map.insert(TokenType::KeywordAs, b"as");
        map.insert(TokenType::KeywordAsync, b"async");
        map.insert(TokenType::KeywordAwait, b"await");
        map.insert(TokenType::KeywordBreak, b"break");
        map.insert(TokenType::KeywordCase, b"case");
        map.insert(TokenType::KeywordCatch, b"catch");
        map.insert(TokenType::KeywordClass, b"class");
        map.insert(TokenType::KeywordConst, b"const");
        map.insert(TokenType::KeywordConstructor, b"constructor");
        map.insert(TokenType::KeywordContinue, b"continue");
        map.insert(TokenType::KeywordDebugger, b"debugger");
        map.insert(TokenType::KeywordDefault, b"default");
        map.insert(TokenType::KeywordDelete, b"delete");
        map.insert(TokenType::KeywordDo, b"do");
        map.insert(TokenType::KeywordElse, b"else");
        map.insert(TokenType::KeywordExport, b"export");
        map.insert(TokenType::KeywordExtends, b"extends");
        map.insert(TokenType::KeywordFinally, b"finally");
        map.insert(TokenType::KeywordFor, b"for");
        map.insert(TokenType::KeywordFrom, b"from");
        map.insert(TokenType::KeywordFunction, b"function");
        map.insert(TokenType::KeywordGet, b"get");
        map.insert(TokenType::KeywordIf, b"if");
        map.insert(TokenType::KeywordImport, b"import");
        map.insert(TokenType::KeywordIn, b"in");
        map.insert(TokenType::KeywordInstanceof, b"instanceof");
        map.insert(TokenType::KeywordLet, b"let");
        map.insert(TokenType::KeywordNew, b"new");
        map.insert(TokenType::KeywordOf, b"of");
        map.insert(TokenType::KeywordReturn, b"return");
        map.insert(TokenType::KeywordSet, b"set");
        map.insert(TokenType::KeywordStatic, b"static");
        map.insert(TokenType::KeywordSuper, b"super");
        map.insert(TokenType::KeywordSwitch, b"switch");
        map.insert(TokenType::KeywordThis, b"this");
        map.insert(TokenType::KeywordThrow, b"throw");
        map.insert(TokenType::KeywordTry, b"try");
        map.insert(TokenType::KeywordTypeof, b"typeof");
        map.insert(TokenType::KeywordVar, b"var");
        map.insert(TokenType::KeywordVoid, b"void");
        map.insert(TokenType::KeywordWhile, b"while");
        map.insert(TokenType::KeywordWith, b"with");
        map.insert(TokenType::KeywordYield, b"yield");
        map.insert(TokenType::LiteralFalse, b"false");
        map.insert(TokenType::LiteralNull, b"null");
        map.insert(TokenType::LiteralTrue, b"true");
        map.insert(TokenType::LiteralUndefined, b"undefined");
        map
    };

    pub static ref KEYWORD_STRS: HashMap<&'static [u8], usize> = {
        HashMap::<&'static [u8], usize>::from_iter(KEYWORDS_MAPPING.values().enumerate().map(|(i, v)| (*v, i)))
    };

    // This has a specific order so that when we use MATCHER, we can find the corresponding TokenType.
    static ref PATTERNS: Vec<(TokenType, &'static [u8])> = {
        let mut patterns: Vec<(TokenType, &'static [u8])> = Vec::new();
        for (&k, &v) in OPERATORS_MAPPING.iter() {
            patterns.push((k, v));
        };
        for (&k, &v) in KEYWORDS_MAPPING.iter() {
          patterns.push((k, &v));
        };
        patterns.push((TokenType::CommentMultiple, b"/*"));
        patterns.push((TokenType::CommentSingle, b"//"));
        for c in ID_START_CHARSTR.chunks(1) {
            patterns.push((TokenType::Identifier, c));
        };
        for c in b"0123456789".chunks(1) {
            patterns.push((TokenType::LiteralNumber, c));
        };
        patterns.push((TokenType::LiteralNumberBin, b"0b"));
        patterns.push((TokenType::LiteralNumberBin, b"0B"));
        patterns.push((TokenType::LiteralNumberHex, b"0x"));
        patterns.push((TokenType::LiteralNumberHex, b"0X"));
        patterns.push((TokenType::LiteralNumberOct, b"0o"));
        patterns.push((TokenType::LiteralNumberOct, b"0O"));
        // Prevent `.` immediately followed by a digit from being recognised as the `.` operator.
        for c in b".0.1.2.3.4.5.6.7.8.9".chunks(2) {
            patterns.push((TokenType::LiteralNumber, c));
        };
        // Prevent `?` immediately followed by a decimal number from being recognised as the `?.` operator.
        for c in b"?.0?.1?.2?.3?.4?.5?.6?.7?.8?.9".chunks(3) {
            patterns.push((TokenType::Question, c));
        };
        patterns.push((TokenType::LiteralString, b"\""));
        patterns.push((TokenType::LiteralString, b"'"));
        patterns.push((TokenType::LiteralTemplatePartString, b"`"));
        patterns
    };

    static ref MATCHER: AhoCorasick = AhoCorasickBuilder::new()
        .anchored(true)
        .dfa(true)
        .match_kind(MatchKind::LeftmostLongest)
        .build(PATTERNS.iter().map(|(_, pat)| pat));

    static ref COMMENT_END: AhoCorasick = AhoCorasick::new(&[b"*/"]);
}

fn lex_multiple_comment(lexer: &mut Lexer) -> TsResult<()> {
    // Consume `/*`.
    lexer.skip_expect(2);
    lexer.consume(lexer.aho_corasick(&COMMENT_END)?.mat);
    Ok(())
}

fn lex_single_comment(lexer: &mut Lexer) -> TsResult<()> {
    // Consume `//`.
    lexer.skip_expect(2);
    // WARNING: Does not consider other line terminators allowed by spec.
    lexer.consume(lexer.through_char(b'\n')?);
    Ok(())
}

fn lex_identifier(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    // Consume starter.
    lexer.skip_expect(1);
    lexer.consume(lexer.while_chars(&ID_CONTINUE));
    Ok(Token::new(
        lexer.since_checkpoint(cp),
        TokenType::Identifier,
        preceded_by_line_terminator,
    ))
}

fn lex_number(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    // TODO
    lexer.consume(lexer.while_chars(&DIGIT));
    lexer.consume(lexer.if_char(b'.'));
    lexer.consume(lexer.while_chars(&DIGIT));
    if lexer
        .peek_or_eof(0)
        .filter(|&c| c == b'e' || c == b'E')
        .is_some()
    {
        lexer.skip_expect(1);
        match lexer.peek(0)? {
            b'+' | b'-' => lexer.skip_expect(1),
            _ => {}
        };
        lexer.consume(lexer.while_chars(&DIGIT));
    }
    Ok(Token::new(
        lexer.since_checkpoint(cp),
        TokenType::LiteralNumber,
        preceded_by_line_terminator,
    ))
}

fn lex_number_bin(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    lexer.skip_expect(2);
    lexer.consume(lexer.while_chars(&DIGIT_BIN));
    Ok(Token::new(
        lexer.since_checkpoint(cp),
        TokenType::LiteralNumber,
        preceded_by_line_terminator,
    ))
}

fn lex_number_hex(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    lexer.skip_expect(2);
    lexer.consume(lexer.while_chars(&DIGIT_HEX));
    Ok(Token::new(
        lexer.since_checkpoint(cp),
        TokenType::LiteralNumber,
        preceded_by_line_terminator,
    ))
}

fn lex_number_oct(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    lexer.skip_expect(2);
    lexer.consume(lexer.while_chars(&DIGIT_OCT));
    Ok(Token::new(
        lexer.since_checkpoint(cp),
        TokenType::LiteralNumber,
        preceded_by_line_terminator,
    ))
}

// TODO Validate regex.
fn lex_regex(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    // Consume slash.
    lexer.consume(lexer.n(1)?);
    let mut in_charset = false;
    loop {
        // WARNING: Does not consider other line terminators allowed by spec.
        match lexer.peek(0)? {
            b'\\' => {
                lexer.skip_expect(1);
                // Cannot escape line terminator.
                // WARNING: Does not consider other line terminators allowed by spec.
                if lexer.peek(1)? == b'\n' {
                    return Err(lexer.error(SyntaxErrorType::LineTerminatorInRegex));
                };
                lexer.skip_expect(1);
            }
            b'/' if !in_charset => {
                lexer.skip_expect(1);
                break;
            }
            b'[' => {
                lexer.skip_expect(1);
                in_charset = true;
            }
            b']' if in_charset => {
                lexer.skip_expect(1);
                in_charset = false;
            }
            b'\n' => {
                return Err(lexer.error(SyntaxErrorType::LineTerminatorInRegex));
            }
            _ => lexer.skip_expect(1),
        };
    }
    lexer.consume(lexer.while_chars(&ID_CONTINUE));
    Ok(Token::new(
        lexer.since_checkpoint(cp),
        TokenType::LiteralRegex,
        preceded_by_line_terminator,
    ))
}

// TODO Validate string.
fn lex_string(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    let quote = lexer.peek(0)?;
    lexer.skip_expect(1);
    loop {
        // WARNING: Does not consider other line terminators allowed by spec.
        lexer.consume(lexer.while_not_3_chars(b'\\', b'\n', quote));
        match lexer.peek(0)? {
            b'\\' => {
                lexer.consume(lexer.n(2)?);
            }
            b'\n' => {
                return Err(lexer.error(SyntaxErrorType::LineTerminatorInString));
            }
            c if c == quote => {
                lexer.skip_expect(1);
                break;
            }
            _ => unreachable!(),
        };
    }
    Ok(Token::new(
        lexer.since_checkpoint(cp),
        TokenType::LiteralString,
        preceded_by_line_terminator,
    ))
}

pub fn lex_template_string_continue(
    lexer: &mut Lexer,
    preceded_by_line_terminator: bool,
) -> TsResult<Token> {
    let cp = lexer.checkpoint();
    let mut loc: Option<SourceRange> = None;
    let mut ended = false;
    loop {
        lexer.consume(lexer.while_not_3_chars(b'\\', b'`', b'$'));
        match lexer.peek(0)? {
            b'\\' => {
                lexer.consume(lexer.n(2)?);
            }
            b'`' => {
                ended = true;
                loc = Some(lexer.since_checkpoint(cp));
                lexer.skip_expect(1);
                break;
            }
            b'$' => {
                if lexer.peek(1)? == b'{' {
                    loc = Some(lexer.since_checkpoint(cp));
                    lexer.skip_expect(2);
                    break;
                } else {
                    lexer.skip_expect(1);
                }
            }
            _ => unreachable!(),
        };
    }
    Ok(Token::new(
        loc.unwrap(),
        if ended {
            TokenType::LiteralTemplatePartStringEnd
        } else {
            TokenType::LiteralTemplatePartString
        },
        preceded_by_line_terminator,
    ))
}

// TODO Validate template.
fn lex_template(lexer: &mut Lexer, preceded_by_line_terminator: bool) -> TsResult<Token> {
    // Consume backtick.
    lexer.skip_expect(1);
    lex_template_string_continue(lexer, preceded_by_line_terminator)
}

pub fn lex_next(lexer: &mut Lexer, mode: LexMode) -> TsResult<Token> {
    let mut preceded_by_line_terminator = false;
    loop {
        let ws = lexer.while_chars(&WHITESPACE);
        lexer.consume(ws);
        // If we are not in the first loop, we've skipped some comments, so preserve preceded_by_line_terminator set before any previous comment.
        // WARNING: Does not consider other line terminators allowed by spec.
        preceded_by_line_terminator =
            preceded_by_line_terminator || memchr(b'\n', &lexer[ws]).is_some();

        if lexer.at_end() {
            return Ok(Token::new(
                lexer.eof_range(),
                TokenType::EOF,
                preceded_by_line_terminator,
            ));
        };

        let AhoCorasickMatch { id, mut mat } = lexer.aho_corasick(&MATCHER)?;
        match PATTERNS[id].0 {
            TokenType::CommentMultiple => lex_multiple_comment(lexer)?,
            TokenType::CommentSingle => lex_single_comment(lexer)?,
            pat => {
                return match pat {
                    TokenType::Identifier => lex_identifier(lexer, preceded_by_line_terminator),
                    TokenType::LiteralNumber => lex_number(lexer, preceded_by_line_terminator),
                    TokenType::LiteralNumberBin => {
                        lex_number_bin(lexer, preceded_by_line_terminator)
                    }
                    TokenType::LiteralNumberHex => {
                        lex_number_hex(lexer, preceded_by_line_terminator)
                    }
                    TokenType::LiteralNumberOct => {
                        lex_number_oct(lexer, preceded_by_line_terminator)
                    }
                    TokenType::LiteralString => lex_string(lexer, preceded_by_line_terminator),
                    TokenType::LiteralTemplatePartString => {
                        lex_template(lexer, preceded_by_line_terminator)
                    }
                    TokenType::Slash if mode == LexMode::SlashIsRegex => {
                        lex_regex(lexer, preceded_by_line_terminator)
                    }
                    mut typ => {
                        if typ == TokenType::ChevronLeft
                            && ID_CONTINUE_OR_PARENTHESIS_CLOSE_OR_BRACKET_CLOSE
                                .has(lexer.prev_char())
                        {
                            typ = TokenType::ChevronLeftAsTypeArgumentsList;
                        } else if typ == TokenType::Question && mat.len() != 1 {
                            // We've matched `?.[0-9]`.
                            mat = mat.prefix(1);
                        } else if KEYWORDS_MAPPING.contains_key(&typ)
                            && lexer
                                .peek_or_eof(mat.len())
                                .filter(|c| ID_CONTINUE.has(*c))
                                .is_some()
                        {
                            // We've accidentally matched a prefix of an identifier as a keyword.
                            return lex_identifier(lexer, preceded_by_line_terminator);
                        };
                        let loc = lexer.range(mat);
                        lexer.consume(mat);
                        Ok(Token::new(loc, typ, preceded_by_line_terminator))
                    }
                };
            }
        };
    }
}
