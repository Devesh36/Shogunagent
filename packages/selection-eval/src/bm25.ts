/**
 * BM25 with the same tokenizer, stemmer and constants as the router's `discover`
 * (apps/router/src/bm25.ts), so the mock picker ranks the way the router does. Kept as a copy
 * on purpose: the eval must not import product code, or a product change could move the mock
 * and the gate together.
 */

const K1 = 1.2;
const B = 0.75;

const STOPWORDS = new Set([
  "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "by", "at", "from", "as", "is", "are",
  "was", "were", "be", "been", "it", "its", "this", "that", "these", "those", "what", "which", "who", "whom",
  "did", "do", "does", "has", "have", "had", "i", "me", "my", "we", "our", "you", "your", "he", "she", "they",
  "them", "their", "about", "into", "than", "then", "so", "if", "not", "no", "can", "could", "should", "would",
  "will", "get", "find", "give", "show", "tell", "list", "please",
]);

export function stem(token: string): string {
  let t = token;
  for (const suffix of ["ments", "ment", "ations", "ation", "ings", "ing", "ies", "ers", "er", "ed", "es", "s"]) {
    if (t.length > suffix.length + 2 && t.endsWith(suffix)) {
      t = t.slice(0, -suffix.length);
      if (suffix === "ies") t += "y";
      break;
    }
  }
  if (t.length > 3 && t[t.length - 1] === t[t.length - 2]) t = t.slice(0, -1);
  return t;
}

export function tokenize(text: string): string[] {
  return text
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((w) => w.length > 1 && !STOPWORDS.has(w))
    .map(stem);
}

export interface Bm25Doc {
  id: string;
  text: string;
}

export class Bm25Index {
  readonly #tf = new Map<string, Map<string, number>>();
  readonly #df = new Map<string, number>();
  readonly #len = new Map<string, number>();
  readonly #n: number;
  readonly #avgLen: number;

  constructor(docs: Bm25Doc[]) {
    let total = 0;
    for (const d of docs) {
      const tokens = tokenize(d.text);
      const tf = new Map<string, number>();
      for (const t of tokens) tf.set(t, (tf.get(t) ?? 0) + 1);
      for (const t of tf.keys()) this.#df.set(t, (this.#df.get(t) ?? 0) + 1);
      this.#tf.set(d.id, tf);
      this.#len.set(d.id, tokens.length);
      total += tokens.length;
    }
    this.#n = docs.length;
    this.#avgLen = docs.length ? total / docs.length : 1;
  }

  idf(term: string): number {
    const n = this.#df.get(term) ?? 0;
    return Math.log(1 + (this.#n - n + 0.5) / (n + 0.5));
  }

  score(docId: string, queryTerms: string[]): number {
    const tf = this.#tf.get(docId);
    if (!tf) return 0;
    const dl = this.#len.get(docId) ?? 0;
    let s = 0;
    for (const term of queryTerms) {
      const f = tf.get(term) ?? 0;
      if (f === 0) continue;
      s += this.idf(term) * ((f * (K1 + 1)) / (f + K1 * (1 - B + (B * dl) / this.#avgLen)));
    }
    return s;
  }

  bound(queryTerms: string[]): number {
    let s = 0;
    for (const term of queryTerms) s += this.idf(term) * (K1 + 1);
    return s;
  }

  /** Normalised fit in [0, 1). */
  fit(docId: string, query: string): number {
    const terms = [...new Set(tokenize(query))];
    const bound = this.bound(terms);
    if (bound === 0) return 0;
    return this.score(docId, terms) / bound;
  }
}
