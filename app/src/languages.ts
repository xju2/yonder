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
