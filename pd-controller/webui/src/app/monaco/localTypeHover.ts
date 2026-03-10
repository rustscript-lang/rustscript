import type * as Monaco from "monaco-editor";

import { localTypeHintsWithWasm, type LocalTypeHint } from "@/app/lint/wasmLinter";
import type { SourceFlavor } from "@/app/types";

const localTypeHintCache = new Map<string, Promise<LocalTypeHint[]>>();

export function lookupVisibleLocalTypeHint(
  hints: LocalTypeHint[],
  name: string,
  line: number
): LocalTypeHint | null {
  let best: LocalTypeHint | null = null;
  for (const hint of hints) {
    if (hint.name !== name) {
      continue;
    }
    if (hint.declaredLine !== null && line < hint.declaredLine) {
      continue;
    }
    if (hint.lastLine !== null && line > hint.lastLine) {
      continue;
    }
    if (!best) {
      best = hint;
      continue;
    }

    const bestDeclared = best.declaredLine ?? 0;
    const hintDeclared = hint.declaredLine ?? 0;
    if (hintDeclared > bestDeclared) {
      best = hint;
      continue;
    }
    if (hintDeclared === bestDeclared) {
      const bestLast = best.lastLine ?? Number.MAX_SAFE_INTEGER;
      const hintLast = hint.lastLine ?? Number.MAX_SAFE_INTEGER;
      if (hintLast < bestLast) {
        best = hint;
      }
    }
  }
  return best;
}

export async function getLocalTypeHints(
  cacheKey: string,
  source: string,
  flavor: SourceFlavor
): Promise<LocalTypeHint[]> {
  const cached = localTypeHintCache.get(cacheKey);
  if (cached) {
    return cached;
  }

  const pending = localTypeHintsWithWasm(source, flavor).catch((error) => {
    localTypeHintCache.delete(cacheKey);
    throw error;
  });
  localTypeHintCache.set(cacheKey, pending);
  return pending;
}

export async function lookupLocalTypeHover(
  model: Monaco.editor.ITextModel,
  position: Monaco.Position,
  source: string,
  flavor: SourceFlavor,
  cacheKey: string,
  monaco: typeof import("monaco-editor")
): Promise<{ hint: LocalTypeHint; hover: Monaco.languages.Hover } | null> {
  const word = model.getWordAtPosition(position);
  if (!word || !/^[A-Za-z_][A-Za-z0-9_]*$/.test(word.word)) {
    return null;
  }

  const hints = await getLocalTypeHints(cacheKey, source, flavor);
  const hint = lookupVisibleLocalTypeHint(hints, word.word, position.lineNumber);
  if (!hint) {
    return null;
  }

  return {
    hint,
    hover: {
      range: new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, word.endColumn),
      contents: [{ value: `Inferred type: \`${hint.inferredType}\`` }]
    }
  };
}
