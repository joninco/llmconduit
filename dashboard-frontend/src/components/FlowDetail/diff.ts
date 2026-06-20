/**
 * Hand-rolled STRUCTURAL diff (D10 §3.3 constraint: per-JSON-path, no heavy dep).
 *
 * The inspector shows three layers of the SAME logical request as it is transformed:
 *   A = raw inbound body      → B = normalized Responses    → C = upstream chat body
 * To tint what the gateway changed, we compare the layer to the PREVIOUS one and classify
 * every JSON path as added / removed / changed / unchanged. The classification is keyed by a
 * canonical path string (`$.a.b[0].c`) so a `JsonPane` rendering the *right-hand* layer can
 * look up the tint for the value it is about to print, and a pane rendering the *left-hand*
 * layer can show what was removed.
 *
 * "Structural" = we walk the parsed JSON tree by path, NOT a textual line diff. Reordered
 * object keys therefore do NOT register as a change (objects are compared key-wise); only a
 * value/shape difference at a path does. Arrays are compared positionally (index = path
 * segment) — a reorder is a change, matching how a reader reasons about `messages[0]`.
 */

/**
 * How a path in the RIGHT layer relates to the same path in the LEFT layer.
 *
 * The COMPOSITE kinds are produced only by `combineMiddleDiff` for pane B, where a path can carry
 * TWO directional classifications at once (finding 5): introduced/changed by A→B AND dropped on the
 * way to C. `added-removed` = added in B, removed toward C; `changed-removed` = changed in B,
 * removed toward C. They let pane B render BOTH indicators rather than discarding the B→C removal.
 */
export type DiffKind =
  | 'added'
  | 'removed'
  | 'changed'
  | 'unchanged'
  | 'added-removed'
  | 'changed-removed';

/**
 * A flat path→kind map for one layer comparison. Keys are canonical JSON paths
 * (`$`, `$.model`, `$.messages[0].role`). `removed` entries describe paths that exist in the
 * LEFT layer but not the RIGHT — they are surfaced so the left pane can tint a dropped field.
 */
export type DiffMap = ReadonlyMap<string, DiffKind>;

/** Root path token; every other path is built by appending a key or `[i]` segment. */
export const ROOT_PATH = '$';

/**
 * A key is "simple" when it contains none of the path metacharacters (`.`, `[`, `]`) and is
 * non-empty. Simple keys append as the readable `.key` form; any other key (one carrying a dot
 * or bracket, or empty) is bracket + JSON-string encoded as `["<json>"]` so it cannot collide
 * with the nested-path syntax — `{"a.b":…}` and `{"a":{"b":…}}` get DISTINCT paths (finding 6).
 */
function isSimpleKey(key: string): boolean {
  return key.length > 0 && !/[.[\]]/.test(key);
}

/**
 * Appends an object-key segment to a path. Simple keys → `$.model`; keys containing a
 * metacharacter (or empty) → bracket/JSON-string encoded `$["a.b"]` so dotted/bracketed keys
 * stay unambiguous (finding 6). `jsonLines` imports this so serialization + diff classification
 * key paths IDENTICALLY (no second encoder to drift).
 */
export function pathKey(base: string, key: string): string {
  return isSimpleKey(key) ? `${base}.${key}` : `${base}[${JSON.stringify(key)}]`;
}

/** Appends an array-index segment to a path (`$.messages` + 0 → `$.messages[0]`). */
export function pathIndex(base: string, index: number): string {
  return `${base}[${index}]`;
}

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/**
 * Two JSON values are "leaf-equal" when they are the same primitive (or both `null`). Objects
 * and arrays are never leaf-equal here — they recurse so a nested change is located precisely.
 */
function leafEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  // NaN !== NaN by ===; treat equal NaNs as equal so a NaN value isn't a phantom change.
  if (typeof a === 'number' && typeof b === 'number') {
    return Number.isNaN(a) && Number.isNaN(b);
  }
  return false;
}

/**
 * Compares `right` against `left` and records the kind for every path reachable in EITHER.
 * Mutates `out` (so a single map accumulates the whole tree). Both subtrees are walked so that
 * keys present only on the left are recorded as `removed` and keys only on the right as
 * `added`. A container (object/array) whose type matches recurses; a type change (object→array,
 * array→scalar, …) at a path is a single `changed` at that path and its children are NOT walked
 * (the whole subtree was replaced, so per-child tints would be noise).
 */
function walk(path: string, left: unknown, right: unknown, leftHas: boolean, rightHas: boolean, out: Map<string, DiffKind>): void {
  if (leftHas && !rightHas) {
    // The WHOLE left subtree was dropped: mark this path AND every descendant `removed` so the
    // left pane tints every line of the gone subtree, not just its opening brace (finding 3).
    markSubtree(path, left, 'removed', out);
    return;
  }
  if (!leftHas && rightHas) {
    // The whole right subtree is new: mark this path + every descendant `added` (finding 3).
    markSubtree(path, right, 'added', out);
    return;
  }
  // Present in both — compare shape then content.
  if (isPlainObject(left) && isPlainObject(right)) {
    out.set(path, containerKind(left, right, objectChildKinds));
    const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
    for (const key of keys) {
      walk(
        pathKey(path, key),
        left[key],
        right[key],
        Object.prototype.hasOwnProperty.call(left, key),
        Object.prototype.hasOwnProperty.call(right, key),
        out,
      );
    }
    return;
  }
  if (Array.isArray(left) && Array.isArray(right)) {
    out.set(path, containerKind(left, right, arrayChildKinds));
    const len = Math.max(left.length, right.length);
    for (let i = 0; i < len; i++) {
      walk(pathIndex(path, i), left[i], right[i], i < left.length, i < right.length, out);
    }
    return;
  }
  // TYPE CHANGE where AT LEAST ONE side is a container (object↔array, OR scalar↔container): there
  // is no positional/key correspondence to recurse, so the node itself is `changed`. Because each
  // side's children render their OWN lines, mark EVERY descendant of the container side(s): the old
  // container's descendants `removed` (left pane) and the new container's descendants `added` (right
  // pane). A scalar side has no descendants, so a scalar↔container swap classifies only the lone
  // container side's interior (findings 3+4). `markSubtree` writes the node tint last → stays
  // `changed`. (`markSubtree` on a scalar is a no-op beyond the node, so scalar↔scalar never enters.)
  if (isContainer(left) || isContainer(right)) {
    if (isContainer(left)) markSubtree(path, left, 'removed', out);
    if (isContainer(right)) markSubtree(path, right, 'added', out);
    out.set(path, 'changed');
    return;
  }
  // Both sides are scalars (scalar↔scalar): a single tint at this path, no descendants.
  out.set(path, leafEqual(left, right) ? 'unchanged' : 'changed');
}

/** A container is a plain object or an array (the two recursable JSON shapes). */
function isContainer(v: unknown): v is Record<string, unknown> | unknown[] {
  return isPlainObject(v) || Array.isArray(v);
}

/**
 * Marks `path` and EVERY descendant path of `value` with `kind`. Used when a whole subtree is
 * added or removed (present on only one side): the parent alone is not enough, because `JsonPane`
 * tints per serialized LINE and the subtree's children each render their own line. Walking the
 * value with the SAME path scheme (`pathKey`/`pathIndex`) as `jsonLines` guarantees every emitted
 * line finds its tint (finding 3).
 */
function markSubtree(path: string, value: unknown, kind: DiffKind, out: Map<string, DiffKind>): void {
  out.set(path, kind);
  if (isPlainObject(value)) {
    for (const key of Object.keys(value)) {
      markSubtree(pathKey(path, key), value[key], kind, out);
    }
  } else if (Array.isArray(value)) {
    value.forEach((el, i) => markSubtree(pathIndex(path, i), el, kind, out));
  }
}

/**
 * A container node's own kind = `unchanged` if every child path is unchanged, else `changed`.
 * Computed by a shallow probe (the recursive walk fills children regardless); this keeps a
 * parent tinted whenever anything beneath it differs, which is what a reader scanning the
 * collapsed top level expects.
 */
function containerKind(
  left: Record<string, unknown> | unknown[],
  right: Record<string, unknown> | unknown[],
  childKinds: (l: Record<string, unknown> | unknown[], r: Record<string, unknown> | unknown[]) => boolean,
): DiffKind {
  return childKinds(left, right) ? 'unchanged' : 'changed';
}

/** True when two objects are deeply equal (used only for the parent's own tint). */
function objectChildKinds(left: Record<string, unknown> | unknown[], right: Record<string, unknown> | unknown[]): boolean {
  return deepEqual(left, right);
}
function arrayChildKinds(left: Record<string, unknown> | unknown[], right: Record<string, unknown> | unknown[]): boolean {
  return deepEqual(left, right);
}

/** Structural deep-equality (objects compared key-wise, arrays positionally). */
export function deepEqual(a: unknown, b: unknown): boolean {
  if (leafEqual(a, b)) return true;
  if (isPlainObject(a) && isPlainObject(b)) {
    const ak = Object.keys(a);
    const bk = Object.keys(b);
    if (ak.length !== bk.length) return false;
    return ak.every((k) => Object.prototype.hasOwnProperty.call(b, k) && deepEqual(a[k], b[k]));
  }
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((v, i) => deepEqual(v, b[i]));
  }
  return false;
}

/**
 * Builds the path→kind map for `right` relative to `left`. The result tints the RIGHT pane
 * (added/changed paths) and lets the LEFT pane surface `removed` paths. A missing layer
 * (`undefined` — e.g. body evicted) yields an empty map so panes render untinted rather than
 * flagging the whole document.
 */
export function diffLayers(left: unknown, right: unknown): DiffMap {
  const out = new Map<string, DiffKind>();
  if (left === undefined || right === undefined) return out;
  walk(ROOT_PATH, left, right, true, true, out);
  return out;
}

/**
 * Tints for the MIDDLE pane (B), which sits between two comparisons: it is the RIGHT side of
 * A→B (so its `added`/`changed` paths show how the gateway built B from A) AND the LEFT side of
 * B→C (so a field B carries that C drops must show as `removed`). This overlays the two maps so
 * pane B (side `both`, JsonPane) tints BOTH directions. Without this, pane B never shows B→C
 * removals (finding 4).
 *
 * A path classified by BOTH comparisons keeps BOTH (finding 5): an A→B `added`+B→C `removed`
 * becomes the composite `added-removed` (introduced in B, then dropped toward C) and `changed`+
 * `removed` becomes `changed-removed`, so the B→C removal is no longer DISCARDED when A→B already
 * flagged the path. A B→C removal on a path A→B left unchanged stays a plain `removed`.
 */
export function combineMiddleDiff(ab: DiffMap, bc: DiffMap): DiffMap {
  const out = new Map<string, DiffKind>(ab);
  for (const [path, kind] of bc) {
    if (kind !== 'removed') continue;
    const abKind = out.get(path);
    if (abKind === 'added') out.set(path, 'added-removed');
    else if (abKind === 'changed') out.set(path, 'changed-removed');
    else out.set(path, 'removed'); // A→B did not flag it → a lone B→C removal
  }
  return out;
}
