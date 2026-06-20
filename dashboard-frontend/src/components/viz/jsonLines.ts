/**
 * Pretty-prints a JSON value into an array of LINES, each tagged with the canonical JSON path
 * (`$.messages[0].role`) of the value it introduces. This is what lets `JsonPane` tint each
 * printed line by its per-path `DiffKind` (the structural diff is path-keyed, not line-keyed):
 * the serializer is the bridge between the parsed tree the diff walks and the text highlight.js
 * colors.
 *
 * Mirrors `JSON.stringify(v, null, 2)` layout (2-space indent, key on the opening line) so the
 * rendered text reads like a normal pretty-print, but every line knows its path. Closing
 * brackets inherit the container's path (so a tint on a changed container also covers its
 * closing brace).
 */
import { ROOT_PATH, pathIndex, pathKey } from '../../components/FlowDetail/diff';

export interface JsonLine {
  /** The text of this line, already indented (no trailing newline). */
  text: string;
  /** Canonical path of the value this line introduces (for diff tinting). */
  path: string;
  /** Indent depth (for optional gutter rendering / tests). */
  depth: number;
}

const INDENT = '  ';

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** Renders a scalar (or null) as its JSON literal. */
function scalar(v: unknown): string {
  return JSON.stringify(v) ?? 'null';
}

/**
 * Emits the lines for `value` at `path`. `keyPrefix` is the `"key": ` prefix when this value is
 * an object member (empty for array elements / the root). `trailingComma` appends `,` to the
 * value's LAST line when it is not the final sibling.
 */
function emit(value: unknown, path: string, depth: number, keyPrefix: string, trailingComma: boolean, out: JsonLine[]): void {
  const pad = INDENT.repeat(depth);
  const comma = trailingComma ? ',' : '';

  if (isPlainObject(value)) {
    const keys = Object.keys(value);
    if (keys.length === 0) {
      out.push({ text: `${pad}${keyPrefix}{}${comma}`, path, depth });
      return;
    }
    out.push({ text: `${pad}${keyPrefix}{`, path, depth });
    keys.forEach((k, i) => {
      emit(value[k], pathKey(path, k), depth + 1, `${JSON.stringify(k)}: `, i < keys.length - 1, out);
    });
    out.push({ text: `${pad}}${comma}`, path, depth });
    return;
  }

  if (Array.isArray(value)) {
    if (value.length === 0) {
      out.push({ text: `${pad}${keyPrefix}[]${comma}`, path, depth });
      return;
    }
    out.push({ text: `${pad}${keyPrefix}[`, path, depth });
    value.forEach((el, i) => {
      emit(el, pathIndex(path, i), depth + 1, '', i < value.length - 1, out);
    });
    out.push({ text: `${pad}]${comma}`, path, depth });
    return;
  }

  // Scalar / null.
  out.push({ text: `${pad}${keyPrefix}${scalar(value)}${comma}`, path, depth });
}

/**
 * Serializes a JSON value into path-tagged lines. `undefined` (e.g. an evicted body) yields no
 * lines so the caller can render its own "body evicted" placeholder instead of `undefined`.
 */
export function toJsonLines(value: unknown): JsonLine[] {
  if (value === undefined) return [];
  const out: JsonLine[] = [];
  emit(value, ROOT_PATH, 0, '', false, out);
  return out;
}
