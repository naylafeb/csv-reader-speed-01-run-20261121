// Fast CSV reader: an exact drop-in for csv-reader 1.0.0's readCsv.
//
// Two paths, same answers. The fast path is a scanner compiled to
// WebAssembly (csvscan.wasm, built from rust/src/lib.rs in this repository):
// it finds the records, fields and errors and hands back spans, and this
// module slices the field values out of the text. The scanner is scalar:
// its SIMD chunk loops were removed after they corrupted values on the
// practice set (v128 loads on unaligned input offsets), and the scalar
// wasm is still ~9x the baseline. The slow path below (runJS) is a
// plain-JavaScript parser with the same semantics; it runs when the
// WebAssembly module cannot be loaded, and it is the reference the fast
// path was checked against.

import { readFileSync } from 'node:fs';

const ERR_CODE = ['stray_cr', 'unterminated_quote', 'quote_junk'];

let wasm = null; // the module's exports, or false when it cannot be loaded
let wasmTried = false;

function loadWasm() {
  if (wasmTried) return wasm;
  wasmTried = true;
  try {
    const bytes = readFileSync(new URL('./csvscan.wasm', import.meta.url));
    const mod = new WebAssembly.Module(bytes);
    wasm = new WebAssembly.Instance(mod, {}).exports;
  } catch {
    wasm = false;
  }
  return wasm;
}

/** csv-reader 1.0.0's answer for one case's input, via the SIMD scanner. */
function runWasm(input) {
  const ex = wasm;
  const text = String(input.text);
  const header = input.header === true;
  const mem = ex.memory;
  const len = text.length; // u16 code units
  const need = ex.needed_bytes(len);
  const pages = (need + 65535) >> 16;
  const cur = mem.buffer.byteLength >> 16;
  if (pages > cur) mem.grow(pages - cur);
  // The input, as UTF-16LE, straight into the module's memory.
  const target = Buffer.from(mem.buffer, 64, len * 2);
  target.write(text, 0, len * 2, 'utf16le');
  ex.parse(len, header ? 1 : 0);

  const h = new Uint32Array(mem.buffer, 32, 8);
  const nFields = h[0];
  const nRecords = h[1];
  const nErrors = h[2];
  const fieldsPtr = h[3];
  const recordsPtr = h[4];
  const errorsPtr = h[5];
  const valuesPtr = h[6];
  const valuesLen = h[7];
  const valuesStr =
    valuesLen > 0 ? Buffer.from(mem.buffer, valuesPtr, valuesLen * 2).toString('utf16le') : '';
  const fu = new Uint32Array(mem.buffer, fieldsPtr, nFields * 3);
  const ru = new Uint32Array(mem.buffer, recordsPtr, nRecords * 9);
  const eu = new Uint32Array(mem.buffer, errorsPtr, nErrors);

  let columns = null;
  let width = -1;
  const rows = [];
  const errors = [];
  for (let ri = 0; ri < nRecords; ri++) {
    const o = ri * 9;
    const line = ru[o];
    const rowRaw = ru[o + 1];
    const row = rowRaw > 0x7fffffff ? rowRaw - 0x100000000 : rowRaw;
    const first = ru[o + 2];
    const fc = ru[o + 3];
    const es = ru[o + 4];
    const ec = ru[o + 5];
    const werr = ru[o + 6];
    const wpay = ru[o + 7];
    const flags = ru[o + 8];
    const fields = new Array(fc);
    for (let j = 0; j < fc; j++) {
      const fo = (first + j) * 3;
      const s = fu[fo];
      const e = fu[fo + 1];
      const fl = fu[fo + 2];
      fields[j] = fl & 1 ? valuesStr.slice(s, e) : text.slice(s, e);
    }
    if (flags & 1) {
      // Skipped record: its errors stay, it takes no row.
      for (let k = 0; k < ec; k++) {
        errors.push({ code: ERR_CODE[eu[es + k]], line, row: row === -1 ? null : row });
      }
      continue;
    }
    if (width < 0) width = fc;
    if (flags & 2) {
      // Header record.
      columns = fields;
      for (let k = 0; k < ec; k++) {
        errors.push({ code: ERR_CODE[eu[es + k]], line, row: null });
      }
      continue;
    }
    for (let k = 0; k < ec; k++) {
      errors.push({ code: ERR_CODE[eu[es + k]], line, row });
    }
    if (werr === 1) {
      errors.push({ code: 'short_row', line, row, fields: wpay });
      while (fields.length < width) fields.push('');
    } else if (werr === 2) {
      errors.push({ code: 'long_row', line, row, extra: fields.splice(width) });
    }
    rows.push(fields);
  }
  return { columns: header ? (columns ?? []) : null, rows, errors };
}

/** csv-reader 1.0.0's answer for one case's input. */
export function run(input) {
  const ex = loadWasm();
  if (ex) {
    try {
      return runWasm(input);
    } catch {
      // Fall through to the plain-JavaScript parser below.
    }
  }
  return runJS(input);
}

// ---------------------------------------------------------------------------
// Plain-JavaScript parser: the same answers, without WebAssembly. It scans
// with indexOf (C++ memchr-speed search) instead of a per-character loop;
// fields are slices, never built a character at a time. Every branch mirrors
// vendor/csv-reader/index.js of the house baseline exactly (see the header
// comment of the fast path's Rust source for the policy).
// ---------------------------------------------------------------------------

const COMMA = 0;
const LINE_BREAK = 1;
const END = 2;

/** csv-reader 1.0.0's answer for one case's input, in plain JavaScript. */
function runJS(input) {
  const text = String(input.text);
  const header = input.header === true;
  const errors = [];
  const rows = [];
  let columns = null;
  let width = -1;
  const n = text.length;
  let pos = 0;
  let line = 1;
  if (n > 0 && text.charCodeAt(0) === 0xfeff) pos = 1;

  // Scratch slots for the field readers below (avoids an allocation per field).
  let fValue = '';
  let fEnded = END;
  let fAdded = 0;

  function err(code, rl, rr) {
    errors.push({ code, line: rl, row: rr });
  }

  // Index of the next comma, LF or CR at or after p (n when there is none).
  function nextDelim(p) {
    let m = n;
    let i = text.indexOf(',', p);
    if (i !== -1 && i < m) m = i;
    i = text.indexOf('\n', p);
    if (i !== -1 && i < m) m = i;
    i = text.indexOf('\r', p);
    if (i !== -1 && i < m) m = i;
    return m;
  }

  // Lines inside a quoted field: every LF counts one, and every CR not
  // followed by LF counts one (a CRLF counts once, via its LF).
  function countQuotedLines(str, s, e) {
    let c = 0;
    let i = str.indexOf('\n', s);
    while (i !== -1 && i < e) {
      c += 1;
      i = str.indexOf('\n', i + 1);
    }
    i = str.indexOf('\r', s);
    while (i !== -1 && i < e) {
      if (i + 1 >= e || str.charCodeAt(i + 1) !== 10) c += 1;
      i = str.indexOf('\r', i + 1);
    }
    return c;
  }

  // Reads unquoted characters onto `base`. Sets fValue / fEnded / fAdded.
  // rl / rr are the current record's line / row (for stray_cr errors).
  function readUnquoted(base, rl, rr) {
    let value = null; // null: value is a single span (or base + one span)
    let added = 0;
    let start = pos;
    for (;;) {
      const m = nextDelim(pos);
      if (m >= n) {
        const span = text.slice(start, n);
        if (value === null) value = base === '' ? span : base + span;
        else if (span !== '') value += span;
        added += n - start;
        pos = n;
        fValue = value;
        fEnded = END;
        fAdded = added;
        return;
      }
      const c = text.charCodeAt(m);
      if (c === 44) {
        // ','
        const span = text.slice(start, m);
        if (value === null) value = base === '' ? span : base + span;
        else if (span !== '') value += span;
        added += m - start;
        pos = m + 1;
        fValue = value;
        fEnded = COMMA;
        fAdded = added;
        return;
      }
      if (c === 10) {
        // LF
        const span = text.slice(start, m);
        if (value === null) value = base === '' ? span : base + span;
        else if (span !== '') value += span;
        added += m - start;
        pos = m + 1;
        line += 1;
        fValue = value;
        fEnded = LINE_BREAK;
        fAdded = added;
        return;
      }
      // CR
      if (m + 1 < n && text.charCodeAt(m + 1) === 10) {
        // CRLF
        const span = text.slice(start, m);
        if (value === null) value = base === '' ? span : base + span;
        else if (span !== '') value += span;
        added += m - start;
        pos = m + 2;
        line += 1;
        fValue = value;
        fEnded = LINE_BREAK;
        fAdded = added;
        return;
      }
      // Stray CR: dropped, error, the field goes on.
      const span = text.slice(start, m);
      if (value === null) value = base === '' ? span : base + span;
      else if (span !== '') value += span;
      added += m - start;
      err('stray_cr', rl, rr);
      pos = m + 1;
      start = pos;
    }
  }

  // Reads a quoted field whose opening quote is at pos. Sets fValue / fEnded.
  function readQuoted(rl, rr) {
    const open = pos;
    const openLine = line;
    pos += 1;
    // Segments of plain content, each [start, end); a doubled quote sits
    // between two segments and becomes one '"' in the value.
    const segs = [];
    let segStart = pos;
    let hasDouble = false;
    for (;;) {
      const q = text.indexOf('"', pos);
      if (q === -1) {
        // The quote never closes: literal, the rest read again as unquoted.
        pos = open + 1;
        line = openLine;
        err('unterminated_quote', rl, rr);
        readUnquoted('"', rl, rr);
        return;
      }
      if (q + 1 < n && text.charCodeAt(q + 1) === 34) {
        // Doubled quote.
        hasDouble = true;
        segs.push(segStart, q);
        pos = q + 2;
        segStart = pos;
        continue;
      }
      // Closing quote at q; the last segment is [segStart, q).
      let value;
      if (!hasDouble) {
        const s = open + 1;
        const e = q;
        const ni = text.indexOf('\n', s);
        const ri = text.indexOf('\r', s);
        if ((ni === -1 || ni >= e) && (ri === -1 || ri >= e)) {
          value = text.slice(s, e);
        } else {
          value = text.slice(s, e).replace(/\r\n?/g, '\n');
          line += countQuotedLines(text, s, e);
        }
      } else {
        segs.push(segStart, q);
        const parts = [];
        for (let i = 0; i < segs.length; i += 2) parts.push(text.slice(segs[i], segs[i + 1]));
        const joined = parts.join('"');
        // Every LF and every lone CR inside the quotes counts one line.
        line += countQuotedLines(joined, 0, joined.length);
        value = joined.indexOf('\r') === -1 ? joined : joined.replace(/\r\n?/g, '\n');
      }
      pos = q + 1;
      if (pos >= n) {
        fValue = value;
        fEnded = END;
        return;
      }
      const c = text.charCodeAt(pos);
      if (c === 44) {
        pos += 1;
        fValue = value;
        fEnded = COMMA;
        return;
      }
      if (c === 10) {
        pos += 1;
        line += 1;
        fValue = value;
        fEnded = LINE_BREAK;
        return;
      }
      if (c === 13 && pos + 1 < n && text.charCodeAt(pos + 1) === 10) {
        pos += 2;
        line += 1;
        fValue = value;
        fEnded = LINE_BREAK;
        return;
      }
      // Junk after the closing quote, appended as it stands.
      readUnquoted(value, rl, rr);
      if (fAdded > 0) err('quote_junk', rl, rr);
      return;
    }
  }

  while (true) {
    // Lines with no characters are no records.
    while (pos < n) {
      const c = text.charCodeAt(pos);
      if (c === 10) {
        pos += 1;
        line += 1;
      } else if (c === 13 && pos + 1 < n && text.charCodeAt(pos + 1) === 10) {
        pos += 2;
        line += 1;
      } else {
        break;
      }
    }
    if (pos >= n) break;

    const recordLine = line;
    const recordRow = header && columns === null ? null : rows.length;
    const fields = [];
    let quoted = false;
    do {
      quoted = text.charCodeAt(pos) === 34;
      if (quoted) readQuoted(recordLine, recordRow);
      else readUnquoted('', recordLine, recordRow);
      fields.push(fValue);
    } while (fEnded === COMMA);

    // A line that held only stray CRs is left with no characters: no record
    // (its stray_cr errors stay).
    if (fields.length === 1 && fields[0] === '' && !quoted) continue;

    if (width < 0) width = fields.length;
    if (header && columns === null) {
      columns = fields;
      continue;
    }
    if (fields.length < width) {
      errors.push({ code: 'short_row', line: recordLine, row: recordRow, fields: fields.length });
      while (fields.length < width) fields.push('');
    } else if (fields.length > width) {
      errors.push({ code: 'long_row', line: recordLine, row: recordRow, extra: fields.splice(width) });
    }
    rows.push(fields);
  }
  return { columns: header ? (columns ?? []) : null, rows, errors };
}
