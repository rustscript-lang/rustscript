let schemeLanguageRegistered = false;

export function ensureSchemeLanguage(monaco: typeof import("monaco-editor")): void {
  if (schemeLanguageRegistered) {
    return;
  }

  const languageId = "scheme";
  if (!monaco.languages.getLanguages().some((entry) => entry.id === languageId)) {
    monaco.languages.register({ id: languageId });
  }

  monaco.languages.setMonarchTokensProvider(languageId, {
    tokenizer: {
      root: [
        [/;.*$/, "comment"],
        [/#\|/, "comment", "@blockComment"],
        [/"/, "string", "@string"],
        [/[()]/, "delimiter.parenthesis"],
        [/#t|#f|true|false|null/, "keyword"],
        [/\b(?:define|set!|lambda|let|if|cond|begin|quote|import|require|and|or|not)\b/, "keyword"],
        [/[+-]?\d+(?:\.\d+)?/, "number"],
        [/[A-Za-z_+\-*/<>=!?][A-Za-z0-9_+\-*/<>=!?]*/, "identifier"]
      ],
      blockComment: [
        [/#\|/, "comment", "@push"],
        [/\|#/, "comment", "@pop"],
        [/./, "comment"]
      ],
      string: [
        [/\\./, "string.escape"],
        [/"/, "string", "@pop"],
        [/[^\\"]+/, "string"]
      ]
    }
  });

  monaco.languages.setLanguageConfiguration(languageId, {
    comments: {
      lineComment: ";",
      blockComment: ["#|", "|#"]
    },
    brackets: [
      ["(", ")"],
      ["[", "]"],
      ["{", "}"]
    ],
    autoClosingPairs: [
      { open: "(", close: ")" },
      { open: "[", close: "]" },
      { open: "{", close: "}" },
      { open: "\"", close: "\"" }
    ],
    surroundingPairs: [
      { open: "(", close: ")" },
      { open: "[", close: "]" },
      { open: "{", close: "}" },
      { open: "\"", close: "\"" }
    ]
  });

  schemeLanguageRegistered = true;
}
