use ratatui::style::{Color, Modifier, Style};
use std::path::Path;

/// 토큰 유형
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Keyword,
    Type,
    String,
    Number,
    Comment,
    Operator,
    Function,
    Macro,
    Attribute,
    Variable,
    Constant,
    Bracket,
    Normal,
}

/// 언어 유형
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    C,
    Cpp,
    Java,
    Go,
    Html,
    Css,
    Json,
    Yaml,
    Toml,
    Markdown,
    Shell,
    Sql,
    Xml,
    Ruby,
    Php,
    Swift,
    Kotlin,
    Plain,
}

impl Language {
    /// 파일 확장자로 언어 감지
    pub fn from_extension(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        match ext.as_str() {
            "rs" => Language::Rust,
            "py" | "pyw" | "pyi" => Language::Python,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "java" => Language::Java,
            "go" => Language::Go,
            "html" | "htm" => Language::Html,
            "css" | "scss" | "sass" | "less" => Language::Css,
            "json" | "jsonc" => Language::Json,
            "yml" | "yaml" => Language::Yaml,
            "toml" => Language::Toml,
            "md" | "markdown" => Language::Markdown,
            "sh" | "bash" | "zsh" | "fish" => Language::Shell,
            "sql" => Language::Sql,
            "xml" | "xsl" | "xslt" | "svg" => Language::Xml,
            "rb" | "erb" | "rake" => Language::Ruby,
            "php" | "php3" | "php4" | "php5" | "phtml" => Language::Php,
            "swift" => Language::Swift,
            "kt" | "kts" => Language::Kotlin,
            _ => Language::Plain,
        }
    }

    /// 언어 이름 반환
    pub fn name(&self) -> &'static str {
        match self {
            Language::Rust => "Rust",
            Language::Python => "Python",
            Language::JavaScript => "JavaScript",
            Language::TypeScript => "TypeScript",
            Language::C => "C",
            Language::Cpp => "C++",
            Language::Java => "Java",
            Language::Go => "Go",
            Language::Html => "HTML",
            Language::Css => "CSS",
            Language::Json => "JSON",
            Language::Yaml => "YAML",
            Language::Toml => "TOML",
            Language::Markdown => "Markdown",
            Language::Shell => "Shell",
            Language::Sql => "SQL",
            Language::Xml => "XML",
            Language::Ruby => "Ruby",
            Language::Php => "PHP",
            Language::Swift => "Swift",
            Language::Kotlin => "Kotlin",
            Language::Plain => "Plain",
        }
    }
}

/// 문법 강조 색상 스키마
#[derive(Clone, Debug)]
pub struct SyntaxColors {
    pub keyword: Color,
    pub type_color: Color,
    pub string: Color,
    pub number: Color,
    pub comment: Color,
    pub operator: Color,
    pub function: Color,
    pub macro_color: Color,
    pub attribute: Color,
    pub variable: Color,
    pub constant: Color,
    pub bracket: Color,
    pub normal: Color,
}

impl Default for SyntaxColors {
    fn default() -> Self {
        // Dracula-like syntax colors
        Self {
            keyword: Color::Rgb(255, 121, 198),     // Pink
            type_color: Color::Rgb(139, 233, 253), // Cyan
            string: Color::Rgb(241, 250, 140),     // Yellow
            number: Color::Rgb(189, 147, 249),     // Purple
            comment: Color::Rgb(98, 114, 164),     // Comment gray
            operator: Color::Rgb(255, 121, 198),   // Pink
            function: Color::Rgb(80, 250, 123),    // Green
            macro_color: Color::Rgb(139, 233, 253),// Cyan
            attribute: Color::Rgb(80, 250, 123),   // Green
            variable: Color::Rgb(248, 248, 242),   // Foreground
            constant: Color::Rgb(189, 147, 249),   // Purple
            bracket: Color::Rgb(248, 248, 242),    // Foreground
            normal: Color::Rgb(248, 248, 242),     // Foreground
        }
    }
}

impl SyntaxColors {
    /// 256 색상 호환 버전
    pub fn compatible() -> Self {
        Self {
            keyword: Color::Magenta,
            type_color: Color::Cyan,
            string: Color::Yellow,
            number: Color::LightMagenta,
            comment: Color::DarkGray,
            operator: Color::Magenta,
            function: Color::Green,
            macro_color: Color::Cyan,
            attribute: Color::Green,
            variable: Color::White,
            constant: Color::LightMagenta,
            bracket: Color::White,
            normal: Color::White,
        }
    }

    /// 토큰 타입에 따른 스타일 반환
    pub fn style_for(&self, token_type: TokenType) -> Style {
        let color = match token_type {
            TokenType::Keyword => self.keyword,
            TokenType::Type => self.type_color,
            TokenType::String => self.string,
            TokenType::Number => self.number,
            TokenType::Comment => self.comment,
            TokenType::Operator => self.operator,
            TokenType::Function => self.function,
            TokenType::Macro => self.macro_color,
            TokenType::Attribute => self.attribute,
            TokenType::Variable => self.variable,
            TokenType::Constant => self.constant,
            TokenType::Bracket => self.bracket,
            TokenType::Normal => self.normal,
        };

        let mut style = Style::default().fg(color);
        if token_type == TokenType::Comment {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if token_type == TokenType::Keyword {
            style = style.add_modifier(Modifier::BOLD);
        }
        style
    }
}

/// 토큰
#[derive(Debug, Clone)]
pub struct Token {
    pub text: String,
    pub token_type: TokenType,
}

/// 문법 강조기
#[derive(Debug, Clone)]
pub struct SyntaxHighlighter {
    language: Language,
    colors: SyntaxColors,
    in_multiline_comment: bool,
    in_multiline_string: bool,
}

impl SyntaxHighlighter {
    pub fn new(language: Language) -> Self {
        let truecolor = std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false);

        Self {
            language,
            colors: if truecolor {
                SyntaxColors::default()
            } else {
                SyntaxColors::compatible()
            },
            in_multiline_comment: false,
            in_multiline_string: false,
        }
    }

    /// 라인을 토큰화
    pub fn tokenize_line(&mut self, line: &str) -> Vec<Token> {
        match self.language {
            Language::Rust => self.tokenize_rust(line),
            Language::Python => self.tokenize_python(line),
            Language::JavaScript | Language::TypeScript => self.tokenize_javascript(line),
            Language::C | Language::Cpp => self.tokenize_c(line),
            Language::Java | Language::Kotlin => self.tokenize_java(line),
            Language::Go => self.tokenize_go(line),
            Language::Html | Language::Xml => self.tokenize_html(line),
            Language::Css => self.tokenize_css(line),
            Language::Json => self.tokenize_json(line),
            Language::Yaml | Language::Toml => self.tokenize_yaml(line),
            Language::Shell => self.tokenize_shell(line),
            Language::Sql => self.tokenize_sql(line),
            Language::Ruby => self.tokenize_ruby(line),
            Language::Php => self.tokenize_php(line),
            Language::Swift => self.tokenize_swift(line),
            Language::Markdown => self.tokenize_markdown(line),
            Language::Plain => vec![Token {
                text: line.to_string(),
                token_type: TokenType::Normal,
            }],
        }
    }

    /// 토큰에 대한 스타일 가져오기
    pub fn style_for(&self, token_type: TokenType) -> Style {
        self.colors.style_for(token_type)
    }

    /// 상태 리셋
    pub fn reset(&mut self) {
        self.in_multiline_comment = false;
        self.in_multiline_string = false;
    }

    // Rust 토큰화
    fn tokenize_rust(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "as", "async", "await", "break", "const", "continue", "crate", "dyn",
            "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
            "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
            "self", "Self", "static", "struct", "super", "trait", "true", "type",
            "unsafe", "use", "where", "while", "abstract", "become", "box", "do",
            "final", "macro", "override", "priv", "typeof", "unsized", "virtual",
            "yield",
        ];
        let types = [
            "i8", "i16", "i32", "i64", "i128", "isize",
            "u8", "u16", "u32", "u64", "u128", "usize",
            "f32", "f64", "bool", "char", "str", "String",
            "Vec", "Option", "Result", "Box", "Rc", "Arc",
            "HashMap", "HashSet", "BTreeMap", "BTreeSet",
            "Path", "PathBuf", "OsStr", "OsString",
        ];

        self.tokenize_c_like(line, &keywords, &types, "//", ("/*", "*/"), true)
    }

    // Python 토큰화
    fn tokenize_python(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "and", "as", "assert", "async", "await", "break", "class", "continue",
            "def", "del", "elif", "else", "except", "False", "finally", "for",
            "from", "global", "if", "import", "in", "is", "lambda", "None",
            "nonlocal", "not", "or", "pass", "raise", "return", "True", "try",
            "while", "with", "yield",
        ];
        let types = [
            "int", "float", "str", "bool", "list", "dict", "tuple", "set",
            "frozenset", "bytes", "bytearray", "object", "type", "None",
        ];

        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 주석
            if chars[i] == '#' {
                tokens.push(Token {
                    text: chars[i..].iter().collect(),
                    token_type: TokenType::Comment,
                });
                break;
            }

            // 문자열 (triple quotes)
            if i + 2 < chars.len()
                && ((chars[i] == '"' && chars[i+1] == '"' && chars[i+2] == '"')
                    || (chars[i] == '\'' && chars[i+1] == '\'' && chars[i+2] == '\''))
            {
                let quote = chars[i];
                let start = i;
                i += 3;
                while i + 2 < chars.len() {
                    if chars[i] == quote && chars[i+1] == quote && chars[i+2] == quote {
                        i += 3;
                        break;
                    }
                    i += 1;
                }
                if i > chars.len() {
                    i = chars.len();
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 문자열 (single/double quotes)
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // f-string prefix
            if (chars[i] == 'f' || chars[i] == 'r' || chars[i] == 'b')
                && i + 1 < chars.len()
                && (chars[i + 1] == '"' || chars[i + 1] == '\'')
            {
                let prefix = chars[i];
                let quote = chars[i + 1];
                let start = i;
                i += 2;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: format!("{}{}", prefix, chars[start+1..i].iter().collect::<String>()),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() || (chars[i] == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 식별자/키워드
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token_type = if keywords.contains(&word.as_str()) {
                    TokenType::Keyword
                } else if types.contains(&word.as_str()) {
                    TokenType::Type
                } else if word.chars().all(|c| c.is_uppercase() || c == '_') {
                    TokenType::Constant
                } else if i < chars.len() && chars[i] == '(' {
                    TokenType::Function
                } else {
                    TokenType::Variable
                };
                tokens.push(Token {
                    text: word,
                    token_type,
                });
                continue;
            }

            // 데코레이터
            if chars[i] == '@' {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Attribute,
                });
                continue;
            }

            // 연산자
            if "+-*/%=<>!&|^~".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 괄호
            if "()[]{}".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // JavaScript/TypeScript 토큰화
    fn tokenize_javascript(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "break", "case", "catch", "class", "const", "continue", "debugger",
            "default", "delete", "do", "else", "export", "extends", "false",
            "finally", "for", "function", "if", "import", "in", "instanceof",
            "let", "new", "null", "return", "super", "switch", "this", "throw",
            "true", "try", "typeof", "var", "void", "while", "with", "yield",
            "async", "await", "of", "static", "get", "set", "from", "as",
            // TypeScript
            "interface", "type", "enum", "implements", "private", "protected",
            "public", "readonly", "abstract", "declare", "namespace", "module",
        ];
        let types = [
            "string", "number", "boolean", "object", "any", "void", "never",
            "unknown", "undefined", "null", "Array", "Map", "Set", "Promise",
            "Date", "RegExp", "Error", "Function", "Object", "Symbol", "BigInt",
        ];

        self.tokenize_c_like(line, &keywords, &types, "//", ("/*", "*/"), true)
    }

    // C/C++ 토큰화
    fn tokenize_c(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "auto", "break", "case", "char", "const", "continue", "default",
            "do", "double", "else", "enum", "extern", "float", "for", "goto",
            "if", "inline", "int", "long", "register", "restrict", "return",
            "short", "signed", "sizeof", "static", "struct", "switch", "typedef",
            "union", "unsigned", "void", "volatile", "while", "_Bool", "_Complex",
            "_Imaginary",
            // C++
            "alignas", "alignof", "and", "and_eq", "asm", "atomic_cancel",
            "atomic_commit", "atomic_noexcept", "bitand", "bitor", "bool",
            "catch", "char8_t", "char16_t", "char32_t", "class", "compl",
            "concept", "consteval", "constexpr", "constinit", "const_cast",
            "co_await", "co_return", "co_yield", "decltype", "delete",
            "dynamic_cast", "explicit", "export", "false", "friend", "mutable",
            "namespace", "new", "noexcept", "not", "not_eq", "nullptr",
            "operator", "or", "or_eq", "private", "protected", "public",
            "reflexpr", "reinterpret_cast", "requires", "static_assert",
            "static_cast", "synchronized", "template", "this", "thread_local",
            "throw", "true", "try", "typeid", "typename", "using", "virtual",
            "wchar_t", "xor", "xor_eq",
        ];
        let types = [
            "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t",
            "uint32_t", "uint64_t", "size_t", "ssize_t", "ptrdiff_t", "intptr_t",
            "uintptr_t", "FILE", "time_t", "clock_t", "wint_t", "errno_t",
            "nullptr_t",
            // C++ STL
            "string", "vector", "map", "set", "list", "deque", "array",
            "unordered_map", "unordered_set", "pair", "tuple", "optional",
            "variant", "any", "span", "string_view", "unique_ptr", "shared_ptr",
            "weak_ptr",
        ];

        self.tokenize_c_like(line, &keywords, &types, "//", ("/*", "*/"), true)
    }

    // Java/Kotlin 토큰화
    fn tokenize_java(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "abstract", "assert", "boolean", "break", "byte", "case", "catch",
            "char", "class", "const", "continue", "default", "do", "double",
            "else", "enum", "extends", "final", "finally", "float", "for",
            "goto", "if", "implements", "import", "instanceof", "int",
            "interface", "long", "native", "new", "package", "private",
            "protected", "public", "return", "short", "static", "strictfp",
            "super", "switch", "synchronized", "this", "throw", "throws",
            "transient", "try", "void", "volatile", "while", "true", "false",
            "null",
            // Kotlin
            "fun", "val", "var", "when", "object", "companion", "data", "sealed",
            "inline", "crossinline", "noinline", "reified", "suspend", "typealias",
            "by", "init", "constructor", "where", "out", "in", "is", "as",
            "internal", "open", "lateinit", "annotation", "actual", "expect",
        ];
        let types = [
            "String", "Integer", "Long", "Double", "Float", "Boolean", "Byte",
            "Short", "Character", "Object", "Class", "List", "Map", "Set",
            "ArrayList", "HashMap", "HashSet", "LinkedList", "TreeMap", "TreeSet",
            "Optional", "Stream", "Comparable", "Runnable", "Callable", "Future",
            "Thread", "Exception", "RuntimeException", "Error", "Throwable",
            // Kotlin
            "Int", "Any", "Unit", "Nothing", "Array", "Pair", "Triple",
            "Sequence", "MutableList", "MutableMap", "MutableSet",
        ];

        self.tokenize_c_like(line, &keywords, &types, "//", ("/*", "*/"), true)
    }

    // Go 토큰화
    fn tokenize_go(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "break", "case", "chan", "const", "continue", "default", "defer",
            "else", "fallthrough", "for", "func", "go", "goto", "if", "import",
            "interface", "map", "package", "range", "return", "select", "struct",
            "switch", "type", "var", "true", "false", "nil", "iota",
        ];
        let types = [
            "bool", "byte", "complex64", "complex128", "error", "float32",
            "float64", "int", "int8", "int16", "int32", "int64", "rune",
            "string", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr",
        ];

        self.tokenize_c_like(line, &keywords, &types, "//", ("/*", "*/"), false)
    }

    // HTML/XML 토큰화
    fn tokenize_html(&mut self, line: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 주석
            if i + 3 < chars.len() && &line[i..i+4] == "<!--" {
                let start = i;
                i += 4;
                while i + 2 < chars.len() {
                    if &line[i..i+3] == "-->" {
                        i += 3;
                        break;
                    }
                    i += 1;
                }
                if i > chars.len() {
                    i = chars.len();
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Comment,
                });
                continue;
            }

            // 태그
            if chars[i] == '<' {
                let start = i;
                i += 1;

                // 닫는 태그 슬래시
                if i < chars.len() && chars[i] == '/' {
                    i += 1;
                }

                // 태그 이름
                let tag_start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_' || chars[i] == ':') {
                    i += 1;
                }
                let tag_name: String = chars[tag_start..i].iter().collect();

                if !tag_name.is_empty() {
                    tokens.push(Token {
                        text: chars[start..tag_start].iter().collect(),
                        token_type: TokenType::Bracket,
                    });
                    tokens.push(Token {
                        text: tag_name,
                        token_type: TokenType::Keyword,
                    });
                }

                // 속성들
                while i < chars.len() && chars[i] != '>' {
                    // 공백
                    if chars[i].is_whitespace() {
                        let ws_start = i;
                        while i < chars.len() && chars[i].is_whitespace() {
                            i += 1;
                        }
                        tokens.push(Token {
                            text: chars[ws_start..i].iter().collect(),
                            token_type: TokenType::Normal,
                        });
                        continue;
                    }

                    // 속성 이름
                    if chars[i].is_alphabetic() || chars[i] == '_' || chars[i] == ':' {
                        let attr_start = i;
                        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_' || chars[i] == ':') {
                            i += 1;
                        }
                        tokens.push(Token {
                            text: chars[attr_start..i].iter().collect(),
                            token_type: TokenType::Attribute,
                        });
                        continue;
                    }

                    // 등호
                    if chars[i] == '=' {
                        tokens.push(Token {
                            text: "=".to_string(),
                            token_type: TokenType::Operator,
                        });
                        i += 1;
                        continue;
                    }

                    // 속성 값
                    if chars[i] == '"' || chars[i] == '\'' {
                        let quote = chars[i];
                        let str_start = i;
                        i += 1;
                        while i < chars.len() && chars[i] != quote {
                            i += 1;
                        }
                        if i < chars.len() {
                            i += 1;
                        }
                        tokens.push(Token {
                            text: chars[str_start..i].iter().collect(),
                            token_type: TokenType::String,
                        });
                        continue;
                    }

                    // Self-closing slash
                    if chars[i] == '/' {
                        tokens.push(Token {
                            text: "/".to_string(),
                            token_type: TokenType::Bracket,
                        });
                        i += 1;
                        continue;
                    }

                    i += 1;
                }

                // 닫는 괄호
                if i < chars.len() && chars[i] == '>' {
                    tokens.push(Token {
                        text: ">".to_string(),
                        token_type: TokenType::Bracket,
                    });
                    i += 1;
                }
                continue;
            }

            // 텍스트 콘텐츠
            let start = i;
            while i < chars.len() && chars[i] != '<' {
                i += 1;
            }
            if i > start {
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Normal,
                });
            }
        }

        tokens
    }

    // CSS 토큰화
    fn tokenize_css(&mut self, line: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 주석
            if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
                let start = i;
                i += 2;
                while i + 1 < chars.len() {
                    if chars[i] == '*' && chars[i + 1] == '/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Comment,
                });
                continue;
            }

            // 선택자 (. # 로 시작)
            if chars[i] == '.' || chars[i] == '#' {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Function,
                });
                continue;
            }

            // @ 규칙
            if chars[i] == '@' {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Keyword,
                });
                continue;
            }

            // 속성
            if chars[i].is_alphabetic() || chars[i] == '-' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Attribute,
                });
                continue;
            }

            // 문자열
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() || (chars[i] == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '%' || chars[i] == '-') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 괄호
            if "{}()[]".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 연산자
            if ":;,".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // JSON 토큰화
    fn tokenize_json(&mut self, line: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 공백
            if chars[i].is_whitespace() {
                let start = i;
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Normal,
                });
                continue;
            }

            // 문자열 (키 또는 값)
            if chars[i] == '"' {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }

                // 뒤에 콜론이 있으면 키
                let mut is_key = false;
                let mut j = i;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ':' {
                    is_key = true;
                }

                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: if is_key { TokenType::Attribute } else { TokenType::String },
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() || chars[i] == '-' || chars[i] == '+' {
                let start = i;
                if chars[i] == '-' || chars[i] == '+' {
                    i += 1;
                }
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E' || chars[i] == '-' || chars[i] == '+') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 불린/null
            let remaining: String = chars[i..].iter().collect();
            if remaining.starts_with("true") {
                tokens.push(Token {
                    text: "true".to_string(),
                    token_type: TokenType::Keyword,
                });
                i += 4;
                continue;
            }
            if remaining.starts_with("false") {
                tokens.push(Token {
                    text: "false".to_string(),
                    token_type: TokenType::Keyword,
                });
                i += 5;
                continue;
            }
            if remaining.starts_with("null") {
                tokens.push(Token {
                    text: "null".to_string(),
                    token_type: TokenType::Keyword,
                });
                i += 4;
                continue;
            }

            // 괄호
            if "{}[]".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 콜론, 콤마
            if ":,".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // YAML/TOML 토큰화
    fn tokenize_yaml(&mut self, line: &str) -> Vec<Token> {
        let mut tokens = Vec::new();

        // 주석
        if let Some(comment_pos) = line.find('#') {
            // # 이전 부분
            if comment_pos > 0 {
                let before = &line[..comment_pos];
                tokens.extend(self.tokenize_yaml_content(before));
            }
            // 주석 부분
            tokens.push(Token {
                text: line[comment_pos..].to_string(),
                token_type: TokenType::Comment,
            });
            return tokens;
        }

        self.tokenize_yaml_content(line)
    }

    fn tokenize_yaml_content(&self, line: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 키: 값 형태
            if chars[i].is_alphabetic() || chars[i] == '_' || chars[i] == '-' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-' || chars[i] == '.') {
                    i += 1;
                }

                // 뒤에 콜론이 있으면 키
                let mut is_key = false;
                if i < chars.len() && chars[i] == ':' {
                    is_key = true;
                }

                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: if is_key { TokenType::Attribute } else { TokenType::Variable },
                });
                continue;
            }

            // 문자열
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() || ((chars[i] == '-' || chars[i] == '+') && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
                let start = i;
                if chars[i] == '-' || chars[i] == '+' {
                    i += 1;
                }
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 불린
            let remaining: String = chars[i..].iter().collect();
            if remaining.starts_with("true") || remaining.starts_with("false") || remaining.starts_with("yes") || remaining.starts_with("no") || remaining.starts_with("null") {
                let word_len = if remaining.starts_with("false") { 5 } else if remaining.starts_with("true") { 4 } else if remaining.starts_with("null") { 4 } else if remaining.starts_with("yes") { 3 } else { 2 };
                tokens.push(Token {
                    text: chars[i..i + word_len].iter().collect(),
                    token_type: TokenType::Keyword,
                });
                i += word_len;
                continue;
            }

            // 콜론
            if chars[i] == ':' {
                tokens.push(Token {
                    text: ":".to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 대시 (리스트 항목)
            if chars[i] == '-' {
                tokens.push(Token {
                    text: "-".to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 괄호
            if "{}[]".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // Shell 토큰화
    fn tokenize_shell(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "if", "then", "else", "elif", "fi", "case", "esac", "for", "while",
            "until", "do", "done", "in", "function", "select", "time", "coproc",
            "return", "exit", "break", "continue", "local", "declare", "typeset",
            "export", "readonly", "unset", "shift", "source", "alias", "unalias",
            "set", "shopt", "trap", "exec", "eval", "true", "false",
        ];
        let builtins = [
            "echo", "printf", "read", "cd", "pwd", "pushd", "popd", "dirs",
            "let", "test", "[", "[[", "]]", "]", "getopts", "hash", "type",
            "umask", "ulimit", "wait", "jobs", "fg", "bg", "kill", "disown",
            "suspend", "logout", "history", "fc", "bind", "help", "enable",
            "builtin", "command", "compgen", "complete", "compopt", "mapfile",
            "readarray", "coproc",
        ];

        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 주석
            if chars[i] == '#' {
                tokens.push(Token {
                    text: chars[i..].iter().collect(),
                    token_type: TokenType::Comment,
                });
                break;
            }

            // 문자열
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() && quote == '"' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 변수
            if chars[i] == '$' {
                let start = i;
                i += 1;
                if i < chars.len() && chars[i] == '{' {
                    i += 1;
                    while i < chars.len() && chars[i] != '}' {
                        i += 1;
                    }
                    if i < chars.len() {
                        i += 1;
                    }
                } else if i < chars.len() && chars[i] == '(' {
                    let mut depth = 1;
                    i += 1;
                    while i < chars.len() && depth > 0 {
                        if chars[i] == '(' {
                            depth += 1;
                        } else if chars[i] == ')' {
                            depth -= 1;
                        }
                        i += 1;
                    }
                } else {
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Variable,
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 식별자/키워드
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token_type = if keywords.contains(&word.as_str()) {
                    TokenType::Keyword
                } else if builtins.contains(&word.as_str()) {
                    TokenType::Function
                } else {
                    TokenType::Normal
                };
                tokens.push(Token {
                    text: word,
                    token_type,
                });
                continue;
            }

            // 연산자
            if "=|&;<>!+-*/%".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 괄호
            if "()[]{}".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // SQL 토큰화
    fn tokenize_sql(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "IN", "BETWEEN",
            "LIKE", "IS", "NULL", "TRUE", "FALSE", "AS", "ON", "JOIN", "LEFT",
            "RIGHT", "INNER", "OUTER", "FULL", "CROSS", "NATURAL", "USING",
            "GROUP", "BY", "HAVING", "ORDER", "ASC", "DESC", "LIMIT", "OFFSET",
            "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE",
            "TABLE", "INDEX", "VIEW", "DROP", "ALTER", "ADD", "COLUMN",
            "PRIMARY", "KEY", "FOREIGN", "REFERENCES", "UNIQUE", "CHECK",
            "DEFAULT", "CONSTRAINT", "CASCADE", "RESTRICT", "UNION", "ALL",
            "EXCEPT", "INTERSECT", "EXISTS", "CASE", "WHEN", "THEN", "ELSE",
            "END", "IF", "BEGIN", "COMMIT", "ROLLBACK", "TRANSACTION",
            "DECLARE", "CURSOR", "FETCH", "CLOSE", "OPEN", "FOR", "WHILE",
            "LOOP", "RETURN", "FUNCTION", "PROCEDURE", "TRIGGER", "DATABASE",
            "SCHEMA", "GRANT", "REVOKE", "WITH", "RECURSIVE", "DISTINCT",
            "select", "from", "where", "and", "or", "not", "in", "between",
            "like", "is", "null", "true", "false", "as", "on", "join", "left",
            "right", "inner", "outer", "full", "cross", "natural", "using",
            "group", "by", "having", "order", "asc", "desc", "limit", "offset",
            "insert", "into", "values", "update", "set", "delete", "create",
            "table", "index", "view", "drop", "alter", "add", "column",
            "primary", "key", "foreign", "references", "unique", "check",
            "default", "constraint", "cascade", "restrict", "union", "all",
            "except", "intersect", "exists", "case", "when", "then", "else",
            "end", "if", "begin", "commit", "rollback", "transaction",
        ];
        let types = [
            "INT", "INTEGER", "SMALLINT", "BIGINT", "DECIMAL", "NUMERIC",
            "FLOAT", "REAL", "DOUBLE", "PRECISION", "CHAR", "VARCHAR", "TEXT",
            "DATE", "TIME", "TIMESTAMP", "DATETIME", "BOOLEAN", "BOOL", "BLOB",
            "CLOB", "BINARY", "VARBINARY", "UUID", "JSON", "JSONB", "ARRAY",
            "SERIAL", "BIGSERIAL", "MONEY", "INTERVAL",
            "int", "integer", "smallint", "bigint", "decimal", "numeric",
            "float", "real", "double", "precision", "char", "varchar", "text",
            "date", "time", "timestamp", "datetime", "boolean", "bool",
        ];
        let functions = [
            "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "NULLIF",
            "CAST", "CONVERT", "CONCAT", "SUBSTRING", "TRIM", "UPPER", "LOWER",
            "LENGTH", "REPLACE", "ROUND", "FLOOR", "CEIL", "ABS", "NOW",
            "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP", "EXTRACT",
            "DATE_PART", "DATE_TRUNC", "ROW_NUMBER", "RANK", "DENSE_RANK",
            "FIRST_VALUE", "LAST_VALUE", "LAG", "LEAD", "OVER", "PARTITION",
            "count", "sum", "avg", "min", "max", "coalesce", "nullif",
            "cast", "convert", "concat", "substring", "trim", "upper", "lower",
        ];

        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 단일 줄 주석 (--)
            if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] == '-' {
                tokens.push(Token {
                    text: chars[i..].iter().collect(),
                    token_type: TokenType::Comment,
                });
                break;
            }

            // 문자열
            if chars[i] == '\'' {
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if i + 1 < chars.len() && chars[i + 1] == '\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() || (chars[i] == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 식별자/키워드
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token_type = if keywords.contains(&word.as_str()) {
                    TokenType::Keyword
                } else if types.contains(&word.as_str()) {
                    TokenType::Type
                } else if functions.contains(&word.as_str()) {
                    TokenType::Function
                } else {
                    TokenType::Variable
                };
                tokens.push(Token {
                    text: word,
                    token_type,
                });
                continue;
            }

            // 연산자
            if "=<>!+-*/%".contains(chars[i]) {
                let start = i;
                while i < chars.len() && "=<>!".contains(chars[i]) {
                    i += 1;
                    if i - start >= 2 {
                        break;
                    }
                }
                if i == start {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Operator,
                });
                continue;
            }

            // 괄호
            if "()".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // Ruby 토큰화
    fn tokenize_ruby(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "BEGIN", "END", "alias", "and", "begin", "break", "case", "class",
            "def", "defined?", "do", "else", "elsif", "end", "ensure", "false",
            "for", "if", "in", "module", "next", "nil", "not", "or", "redo",
            "rescue", "retry", "return", "self", "super", "then", "true",
            "undef", "unless", "until", "when", "while", "yield", "__FILE__",
            "__LINE__", "__ENCODING__", "attr_reader", "attr_writer",
            "attr_accessor", "private", "protected", "public", "require",
            "require_relative", "include", "extend", "prepend", "raise", "fail",
            "catch", "throw", "lambda", "proc", "loop",
        ];

        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // 주석
            if chars[i] == '#' {
                tokens.push(Token {
                    text: chars[i..].iter().collect(),
                    token_type: TokenType::Comment,
                });
                break;
            }

            // 문자열
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 심볼
            if chars[i] == ':' && i + 1 < chars.len() && (chars[i + 1].is_alphabetic() || chars[i + 1] == '_') {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '?' || chars[i] == '!') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Constant,
                });
                continue;
            }

            // 인스턴스 변수
            if chars[i] == '@' {
                let start = i;
                i += 1;
                if i < chars.len() && chars[i] == '@' {
                    i += 1;
                }
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Variable,
                });
                continue;
            }

            // 글로벌 변수
            if chars[i] == '$' {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Variable,
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 식별자/키워드
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '?' || chars[i] == '!') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token_type = if keywords.contains(&word.as_str()) {
                    TokenType::Keyword
                } else if word.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    TokenType::Type
                } else if i < chars.len() && chars[i] == '(' {
                    TokenType::Function
                } else {
                    TokenType::Variable
                };
                tokens.push(Token {
                    text: word,
                    token_type,
                });
                continue;
            }

            // 연산자
            if "=<>!+-*/%&|^~".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 괄호
            if "()[]{}".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // PHP 토큰화
    fn tokenize_php(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "abstract", "and", "array", "as", "break", "callable", "case",
            "catch", "class", "clone", "const", "continue", "declare", "default",
            "die", "do", "echo", "else", "elseif", "empty", "enddeclare",
            "endfor", "endforeach", "endif", "endswitch", "endwhile", "eval",
            "exit", "extends", "final", "finally", "fn", "for", "foreach",
            "function", "global", "goto", "if", "implements", "include",
            "include_once", "instanceof", "insteadof", "interface", "isset",
            "list", "match", "namespace", "new", "or", "print", "private",
            "protected", "public", "readonly", "require", "require_once",
            "return", "static", "switch", "throw", "trait", "try", "unset",
            "use", "var", "while", "xor", "yield", "yield from",
            "true", "false", "null", "TRUE", "FALSE", "NULL",
            "__CLASS__", "__DIR__", "__FILE__", "__FUNCTION__", "__LINE__",
            "__METHOD__", "__NAMESPACE__", "__TRAIT__",
        ];
        let types = [
            "int", "float", "bool", "string", "array", "object", "callable",
            "iterable", "void", "mixed", "never", "null", "self", "parent",
        ];

        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // PHP 태그
            if i + 4 < chars.len() {
                let slice: String = chars[i..i+5].iter().collect();
                if slice == "<?php" {
                    tokens.push(Token {
                        text: "<?php".to_string(),
                        token_type: TokenType::Keyword,
                    });
                    i += 5;
                    continue;
                }
            }
            if i + 1 < chars.len() && chars[i] == '?' && chars[i + 1] == '>' {
                tokens.push(Token {
                    text: "?>".to_string(),
                    token_type: TokenType::Keyword,
                });
                i += 2;
                continue;
            }

            // 주석
            if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
                tokens.push(Token {
                    text: chars[i..].iter().collect(),
                    token_type: TokenType::Comment,
                });
                break;
            }
            if chars[i] == '#' {
                tokens.push(Token {
                    text: chars[i..].iter().collect(),
                    token_type: TokenType::Comment,
                });
                break;
            }

            // 문자열
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 변수
            if chars[i] == '$' {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Variable,
                });
                continue;
            }

            // 숫자
            if chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 식별자/키워드
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token_type = if keywords.contains(&word.as_str()) {
                    TokenType::Keyword
                } else if types.contains(&word.as_str()) {
                    TokenType::Type
                } else if i < chars.len() && chars[i] == '(' {
                    TokenType::Function
                } else {
                    TokenType::Variable
                };
                tokens.push(Token {
                    text: word,
                    token_type,
                });
                continue;
            }

            // 연산자
            if "=<>!+-*/%&|^~.".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Operator,
                });
                i += 1;
                continue;
            }

            // 괄호
            if "()[]{}".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 기타
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // Swift 토큰화
    fn tokenize_swift(&mut self, line: &str) -> Vec<Token> {
        let keywords = [
            "associatedtype", "class", "deinit", "enum", "extension", "fileprivate",
            "func", "import", "init", "inout", "internal", "let", "open",
            "operator", "private", "protocol", "public", "rethrows", "static",
            "struct", "subscript", "typealias", "var", "break", "case",
            "continue", "default", "defer", "do", "else", "fallthrough", "for",
            "guard", "if", "in", "repeat", "return", "switch", "where", "while",
            "as", "Any", "catch", "false", "is", "nil", "super", "self", "Self",
            "throw", "throws", "true", "try", "async", "await", "actor",
        ];
        let types = [
            "Int", "Int8", "Int16", "Int32", "Int64", "UInt", "UInt8", "UInt16",
            "UInt32", "UInt64", "Float", "Double", "Bool", "String", "Character",
            "Array", "Dictionary", "Set", "Optional", "Result", "Void", "Never",
            "AnyObject", "AnyClass", "Error", "Codable", "Hashable", "Equatable",
            "Comparable", "Identifiable", "View", "ObservableObject",
        ];

        self.tokenize_c_like(line, &keywords, &types, "//", ("/*", "*/"), true)
    }

    // Markdown 토큰화
    fn tokenize_markdown(&mut self, line: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let trimmed = line.trim_start();

        // 헤더
        if trimmed.starts_with('#') {
            let hash_count = trimmed.chars().take_while(|&c| c == '#').count();
            if hash_count <= 6 && trimmed.chars().nth(hash_count) == Some(' ') {
                tokens.push(Token {
                    text: line.to_string(),
                    token_type: TokenType::Keyword,
                });
                return tokens;
            }
        }

        // 코드 블록 시작/끝
        if trimmed.starts_with("```") {
            tokens.push(Token {
                text: line.to_string(),
                token_type: TokenType::String,
            });
            return tokens;
        }

        // 인용문
        if trimmed.starts_with('>') {
            tokens.push(Token {
                text: line.to_string(),
                token_type: TokenType::Comment,
            });
            return tokens;
        }

        // 리스트
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            let indent = line.len() - trimmed.len();
            if indent > 0 {
                tokens.push(Token {
                    text: line[..indent].to_string(),
                    token_type: TokenType::Normal,
                });
            }
            tokens.push(Token {
                text: trimmed[..2].to_string(),
                token_type: TokenType::Operator,
            });
            tokens.push(Token {
                text: trimmed[2..].to_string(),
                token_type: TokenType::Normal,
            });
            return tokens;
        }

        // 번호 리스트
        let mut num_end = 0;
        for (i, c) in trimmed.chars().enumerate() {
            if c.is_ascii_digit() {
                num_end = i + 1;
            } else if c == '.' && num_end > 0 && i == num_end {
                let indent = line.len() - trimmed.len();
                if indent > 0 {
                    tokens.push(Token {
                        text: line[..indent].to_string(),
                        token_type: TokenType::Normal,
                    });
                }
                tokens.push(Token {
                    text: trimmed[..num_end + 1].to_string(),
                    token_type: TokenType::Number,
                });
                if num_end + 1 < trimmed.len() {
                    tokens.push(Token {
                        text: trimmed[num_end + 1..].to_string(),
                        token_type: TokenType::Normal,
                    });
                }
                return tokens;
            } else {
                break;
            }
        }

        // 수평선
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            tokens.push(Token {
                text: line.to_string(),
                token_type: TokenType::Comment,
            });
            return tokens;
        }

        // 인라인 요소 처리
        let mut i = 0;
        while i < chars.len() {
            // 볼드/이탤릭
            if chars[i] == '*' || chars[i] == '_' {
                let marker = chars[i];
                let start = i;
                let mut count = 0;
                while i < chars.len() && chars[i] == marker {
                    count += 1;
                    i += 1;
                }
                let mut found_end = false;
                while i < chars.len() {
                    if chars[i] == marker {
                        let mut end_count = 0;
                        while i < chars.len() && chars[i] == marker {
                            end_count += 1;
                            i += 1;
                        }
                        if end_count >= count {
                            tokens.push(Token {
                                text: chars[start..i].iter().collect(),
                                token_type: TokenType::Attribute,
                            });
                            found_end = true;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                if !found_end {
                    tokens.push(Token {
                        text: chars[start..].iter().collect(),
                        token_type: TokenType::Normal,
                    });
                    break;
                }
                continue;
            }

            // 인라인 코드
            if chars[i] == '`' {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // 링크
            if chars[i] == '[' {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                    if i < chars.len() && chars[i] == '(' {
                        while i < chars.len() && chars[i] != ')' {
                            i += 1;
                        }
                        if i < chars.len() {
                            i += 1;
                        }
                        tokens.push(Token {
                            text: chars[start..i].iter().collect(),
                            token_type: TokenType::Function,
                        });
                        continue;
                    }
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Normal,
                });
                continue;
            }

            // 일반 텍스트
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }

    // C-like 언어 공통 토큰화
    fn tokenize_c_like(
        &mut self,
        line: &str,
        keywords: &[&str],
        types: &[&str],
        line_comment: &str,
        block_comment: (&str, &str),
        support_attributes: bool,
    ) -> Vec<Token> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        // 멀티라인 주석 계속
        if self.in_multiline_comment {
            let end_idx = line.find(block_comment.1);
            if let Some(idx) = end_idx {
                tokens.push(Token {
                    text: line[..idx + block_comment.1.len()].to_string(),
                    token_type: TokenType::Comment,
                });
                self.in_multiline_comment = false;
                i = idx + block_comment.1.len();
            } else {
                tokens.push(Token {
                    text: line.to_string(),
                    token_type: TokenType::Comment,
                });
                return tokens;
            }
        }

        while i < chars.len() {
            // 라인 주석
            if i + line_comment.len() <= chars.len() {
                let slice: String = chars[i..i + line_comment.len()].iter().collect();
                if slice == line_comment {
                    tokens.push(Token {
                        text: chars[i..].iter().collect(),
                        token_type: TokenType::Comment,
                    });
                    break;
                }
            }

            // 블록 주석 시작
            if i + block_comment.0.len() <= chars.len() {
                let slice: String = chars[i..i + block_comment.0.len()].iter().collect();
                if slice == block_comment.0 {
                    let start = i;
                    i += block_comment.0.len();
                    let mut found_end = false;
                    while i + block_comment.1.len() <= chars.len() {
                        let end_slice: String = chars[i..i + block_comment.1.len()].iter().collect();
                        if end_slice == block_comment.1 {
                            i += block_comment.1.len();
                            found_end = true;
                            break;
                        }
                        i += 1;
                    }
                    if !found_end {
                        self.in_multiline_comment = true;
                        i = chars.len();
                    }
                    tokens.push(Token {
                        text: chars[start..i].iter().collect(),
                        token_type: TokenType::Comment,
                    });
                    continue;
                }
            }

            // 문자열
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::String,
                });
                continue;
            }

            // Raw 문자열 (Rust의 r#"..."#)
            if chars[i] == 'r' && i + 1 < chars.len() && (chars[i + 1] == '"' || chars[i + 1] == '#') {
                let start = i;
                i += 1;
                let mut hash_count = 0;
                while i < chars.len() && chars[i] == '#' {
                    hash_count += 1;
                    i += 1;
                }
                if i < chars.len() && chars[i] == '"' {
                    i += 1;
                    loop {
                        while i < chars.len() && chars[i] != '"' {
                            i += 1;
                        }
                        if i >= chars.len() {
                            break;
                        }
                        i += 1;
                        let mut closing_hashes = 0;
                        while i < chars.len() && chars[i] == '#' && closing_hashes < hash_count {
                            closing_hashes += 1;
                            i += 1;
                        }
                        if closing_hashes == hash_count {
                            break;
                        }
                    }
                    tokens.push(Token {
                        text: chars[start..i].iter().collect(),
                        token_type: TokenType::String,
                    });
                    continue;
                }
            }

            // 숫자
            if chars[i].is_ascii_digit()
                || (chars[i] == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
            {
                let start = i;
                // 16진수, 8진수, 2진수
                if chars[i] == '0' && i + 1 < chars.len() {
                    let next = chars[i + 1].to_ascii_lowercase();
                    if next == 'x' || next == 'o' || next == 'b' {
                        i += 2;
                    }
                }
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
                {
                    i += 1;
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Number,
                });
                continue;
            }

            // 식별자/키워드
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();

                // 매크로 체크 (뒤에 ! 가 있으면)
                let is_macro = i < chars.len() && chars[i] == '!';
                if is_macro {
                    i += 1;
                    tokens.push(Token {
                        text: format!("{}!", word),
                        token_type: TokenType::Macro,
                    });
                    continue;
                }

                let token_type = if keywords.contains(&word.as_str()) {
                    TokenType::Keyword
                } else if types.contains(&word.as_str()) {
                    TokenType::Type
                } else if word.chars().all(|c| c.is_uppercase() || c == '_') && word.len() > 1 {
                    TokenType::Constant
                } else if i < chars.len() && chars[i] == '(' {
                    TokenType::Function
                } else {
                    TokenType::Variable
                };
                tokens.push(Token {
                    text: word,
                    token_type,
                });
                continue;
            }

            // 속성 (Rust의 #[...], Java의 @...)
            if support_attributes {
                if chars[i] == '#' && i + 1 < chars.len() && chars[i + 1] == '[' {
                    let start = i;
                    i += 2;
                    let mut depth = 1;
                    while i < chars.len() && depth > 0 {
                        if chars[i] == '[' {
                            depth += 1;
                        } else if chars[i] == ']' {
                            depth -= 1;
                        }
                        i += 1;
                    }
                    tokens.push(Token {
                        text: chars[start..i].iter().collect(),
                        token_type: TokenType::Attribute,
                    });
                    continue;
                }
                if chars[i] == '@' {
                    let start = i;
                    i += 1;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    tokens.push(Token {
                        text: chars[start..i].iter().collect(),
                        token_type: TokenType::Attribute,
                    });
                    continue;
                }
            }

            // 연산자
            if "+-*/%=<>!&|^~?:".contains(chars[i]) {
                let start = i;
                // 복합 연산자
                while i < chars.len() && "+-*/%=<>!&|^~?:".contains(chars[i]) {
                    i += 1;
                    if i - start >= 3 {
                        break;
                    }
                }
                tokens.push(Token {
                    text: chars[start..i].iter().collect(),
                    token_type: TokenType::Operator,
                });
                continue;
            }

            // 괄호
            if "()[]{}".contains(chars[i]) {
                tokens.push(Token {
                    text: chars[i].to_string(),
                    token_type: TokenType::Bracket,
                });
                i += 1;
                continue;
            }

            // 기타 (공백, 세미콜론, 콤마 등)
            tokens.push(Token {
                text: chars[i].to_string(),
                token_type: TokenType::Normal,
            });
            i += 1;
        }

        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_detection() {
        assert_eq!(Language::from_extension(Path::new("test.rs")), Language::Rust);
        assert_eq!(Language::from_extension(Path::new("test.py")), Language::Python);
        assert_eq!(Language::from_extension(Path::new("test.js")), Language::JavaScript);
        assert_eq!(Language::from_extension(Path::new("test.ts")), Language::TypeScript);
        assert_eq!(Language::from_extension(Path::new("test.go")), Language::Go);
        assert_eq!(Language::from_extension(Path::new("test.unknown")), Language::Plain);
    }

    #[test]
    fn test_rust_tokenization() {
        let mut highlighter = SyntaxHighlighter::new(Language::Rust);
        let tokens = highlighter.tokenize_line("fn main() {");
        assert!(tokens.iter().any(|t| t.text == "fn" && t.token_type == TokenType::Keyword));
        assert!(tokens.iter().any(|t| t.text == "main" && t.token_type == TokenType::Function));
    }

    #[test]
    fn test_python_tokenization() {
        let mut highlighter = SyntaxHighlighter::new(Language::Python);
        let tokens = highlighter.tokenize_line("def hello():");
        assert!(tokens.iter().any(|t| t.text == "def" && t.token_type == TokenType::Keyword));
        assert!(tokens.iter().any(|t| t.text == "hello" && t.token_type == TokenType::Function));
    }
}
