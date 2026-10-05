// Highlighting Monaco lacks: CMake and ignore files, plus file names it
// misses for languages it has.

import * as monaco from "monaco-editor";

const { languages } = monaco;

languages.register({
  id: "cpp",
  extensions: [".ipp", ".tpp", ".inl", ".tcc", ".cu", ".cuh", ".c++", ".h++"],
});
languages.register({
  id: "dockerfile",
  filenames: ["Containerfile"],
  filenamePatterns: ["Dockerfile.*", "Containerfile.*", "*.Dockerfile"],
});

languages.register({
  id: "cmake",
  extensions: [".cmake"],
  filenames: ["CMakeLists.txt", "CMakeCache.txt"],
  aliases: ["CMake", "cmake"],
});
languages.setLanguageConfiguration("cmake", {
  comments: { lineComment: "#", blockComment: ["#[[", "]]"] },
  brackets: [["(", ")"]],
  autoClosingPairs: [
    { open: "(", close: ")" },
    { open: '"', close: '"', notIn: ["string"] },
  ],
});
languages.setMonarchTokensProvider("cmake", {
  defaultToken: "",
  tokenPostfix: ".cmake",
  // Commands are case-insensitive; old scripts often write IF(...).
  keywords: [
    "if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile",
    "function", "endfunction", "macro", "endmacro", "block", "endblock",
    "return", "break", "continue",
  ].flatMap((k) => [k, k.toUpperCase()]),
  operators: [
    "NOT", "AND", "OR", "COMMAND", "POLICY", "TARGET", "TEST", "EXISTS", "IS_NEWER_THAN",
    "IS_DIRECTORY", "IS_SYMLINK", "IS_ABSOLUTE", "MATCHES", "LESS", "GREATER", "EQUAL",
    "LESS_EQUAL", "GREATER_EQUAL", "STRLESS", "STRGREATER", "STREQUAL", "STRLESS_EQUAL",
    "STRGREATER_EQUAL", "VERSION_LESS", "VERSION_GREATER", "VERSION_EQUAL",
    "VERSION_LESS_EQUAL", "VERSION_GREATER_EQUAL", "IN_LIST", "DEFINED",
  ],
  tokenizer: {
    root: [
      [/#\[(=*)\[/, { token: "comment", next: "@bracketComment.$1" }],
      [/#.*$/, "comment"],
      [/\[(=*)\[/, { token: "string", next: "@bracketString.$1" }],
      [/"/, "string", "@string"],
      { include: "@variables" },
      [
        /[A-Za-z_][\w]*(?=\s*\()/,
        { cases: { "@keywords": "keyword", "@default": "type.identifier" } },
      ],
      // Upper-case words are a command's keyword arguments (PUBLIC, REQUIRED, …).
      [/\b[A-Z][A-Z0-9_]+\b/, { cases: { "@operators": "keyword", "@default": "constant" } }],
      [/\b(ON|OFF|TRUE|FALSE|YES|NO)\b/, "constant"],
      [/-?\d+(\.\d+)*\b/, "number"],
      [/\w+/, ""],
      [/[()]/, "@brackets"],
    ],
    variables: [
      [/\$(ENV|CACHE)?\{/, "variable", "@variable"],
      [/\$</, "variable", "@genex"],
    ],
    // Generator expressions such as $<$<CONFIG:Debug>:-g> nest.
    genex: [
      { include: "@variables" },
      [/>/, "variable", "@pop"],
      [/[^$>]+|\$/, "variable"],
    ],
    variable: [
      [/\$(ENV|CACHE)?\{/, "variable", "@push"],
      [/\}/, "variable", "@pop"],
      [/[^${}]+/, "variable"],
    ],
    string: [
      { include: "@variables" },
      [/\\./, "string.escape"],
      [/"/, "string", "@pop"],
      [/[^"\\$]+|\$/, "string"],
    ],
    bracketComment: [
      [/\](=*)\]/, { cases: { "$1==$S2": { token: "comment", next: "@pop" }, "@default": "comment" } }],
      [/[^\]]+|\]/, "comment"],
    ],
    bracketString: [
      [/\](=*)\]/, { cases: { "$1==$S2": { token: "string", next: "@pop" }, "@default": "string" } }],
      [/[^\]]+|\]/, "string"],
    ],
  },
});

languages.register({
  id: "ignore",
  filenames: [".gitignore", ".dockerignore", ".npmignore", ".hgignore", ".prettierignore", ".eslintignore"],
  aliases: ["Ignore", "ignore"],
});
languages.setLanguageConfiguration("ignore", { comments: { lineComment: "#" } });
languages.setMonarchTokensProvider("ignore", {
  tokenizer: {
    root: [
      [/^\s*#.*$/, "comment"],
      [/^!/, "keyword"],
      [/\\./, "string.escape"],
      [/\*\*|[*?]|\[[^\]]*\]/, "regexp"],
      [/\//, "delimiter"],
    ],
  },
});

languages.register({
  id: "makefile",
  extensions: [".mk", ".mak"],
  filenames: ["Makefile", "makefile", "GNUmakefile"],
  filenamePatterns: ["Makefile.*", "*.make"],
  aliases: ["Makefile", "make"],
});
languages.setLanguageConfiguration("makefile", {
  comments: { lineComment: "#" },
  brackets: [["(", ")"], ["{", "}"]],
  autoClosingPairs: [
    { open: "(", close: ")" },
    { open: "{", close: "}" },
  ],
});
languages.setMonarchTokensProvider("makefile", {
  defaultToken: "",
  tokenPostfix: ".make",
  directives: [
    "include", "-include", "sinclude", "define", "endef", "ifeq", "ifneq", "ifdef", "ifndef",
    "else", "endif", "export", "unexport", "override", "private", "vpath", "undefine",
  ],
  functions: [
    "subst", "patsubst", "strip", "findstring", "filter", "filter-out", "sort", "word", "words",
    "wordlist", "firstword", "lastword", "dir", "notdir", "suffix", "basename", "addsuffix",
    "addprefix", "join", "wildcard", "realpath", "abspath", "if", "or", "and", "foreach",
    "file", "call", "value", "eval", "origin", "flavor", "error", "warning", "info", "shell", "guile",
  ],
  tokenizer: {
    root: [
      // A recipe line: shell, after a tab.
      // @ (silent), - (ignore errors) and + (run with -n) lead it.
      [/^\t[@+-]+/, "keyword", "@recipe"],
      [/^\t/, "", "@recipe"],
      [/#.*$/, "comment"],
      // NAME = value, and the other assignment operators.
      [/^(\s*)([\w.-]+)(\s*)(::?=|:::=|\?=|\+=|!=|=)/, ["", "variable.name", "", "operator"]],
      // Targets, up to a single colon (not :=).
      [/^[^\s:#=][^:#=]*(?=::?(?!=))/, "type.identifier"],
      [/^\s*-?[a-z]+\b/, { cases: { "@directives": "keyword", "@default": "" } }],
      { include: "@refs" },
      [/\\$/, "string.escape"],
    ],
    recipe: [
      [/^[^\t]/, { token: "@rematch", next: "@pop" }],
      [/^\t[@+-]+/, "keyword"],
      [/#.*$/, "comment"],
      { include: "@refs" },
      [/[^$#]+|./, ""],
    ],
    refs: [
      [/\$[@<^*?+%|$]/, "variable"],
      [/\$[({]/, "variable", "@ref"],
      [/\$\w/, "variable"],
    ],
    // $(name) or $(function args), which nest.
    ref: [
      [/[\w-]+(?=\s)/, { cases: { "@functions": "keyword", "@default": "variable" } }],
      [/[\w.-]+(?=[)}:])/, "variable"],
      { include: "@refs" },
      [/[)}]/, "variable", "@pop"],
      [/[^$)}\s]+/, ""],
      [/\s+|[^)}]/, ""],
    ],
  },
});

languages.register({
  id: "latex",
  extensions: [".tex", ".sty", ".cls", ".ltx", ".dtx", ".bbx", ".cbx"],
  aliases: ["LaTeX", "latex", "TeX"],
});
languages.setLanguageConfiguration("latex", {
  comments: { lineComment: "%" },
  brackets: [["{", "}"], ["[", "]"], ["(", ")"]],
  autoClosingPairs: [
    { open: "{", close: "}" },
    { open: "[", close: "]" },
    { open: "(", close: ")" },
    { open: "$", close: "$", notIn: ["comment"] },
    { open: "`", close: "'" },
  ],
  surroundingPairs: [
    { open: "{", close: "}" },
    { open: "[", close: "]" },
    { open: "(", close: ")" },
    { open: "$", close: "$" },
  ],
});
languages.setMonarchTokensProvider("latex", {
  defaultToken: "",
  tokenPostfix: ".latex",
  tokenizer: {
    root: [
      [/%.*$/, "comment"],
      // \begin{env} / \end{env}: the environment name is a type.
      [/(\\(?:begin|end))(\s*\{)([^}]*)(\})/, ["keyword", "delimiter.curly", "type.identifier", "delimiter.curly"]],
      [/\\verb\*?(.)(?:(?!\1).)*\1/, "string"],
      [/\\\[/, "string", "@displayMath"],
      [/\\\(/, "string", "@inlineMath"],
      [/\$\$/, "string", "@displayDollar"],
      [/\$/, "string", "@inlineDollar"],
      [/\\[a-zA-Z@]+\*?/, "keyword"],
      [/\\./, "string.escape"],
      [/[{}[\]]/, "@brackets"],
      [/[&~]/, "operator"],
    ],
    math: [
      [/%.*$/, "comment"],
      [/\\[a-zA-Z@]+\*?/, "keyword"],
      [/\\./, "string.escape"],
      [/[_^&]/, "operator"],
      [/\d+(\.\d+)?/, "number"],
      [/[^\\%_^&\d$]+|./, "string"],
    ],
    inlineDollar: [[/\$/, "string", "@pop"], { include: "math" }],
    displayDollar: [[/\$\$/, "string", "@pop"], { include: "math" }],
    inlineMath: [[/\\\)/, "string", "@pop"], { include: "math" }],
    displayMath: [[/\\\]/, "string", "@pop"], { include: "math" }],
  },
});
