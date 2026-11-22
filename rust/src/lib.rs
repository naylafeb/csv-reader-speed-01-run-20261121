// csvscan: a CSV structural scanner for csv-reader-speed-01, compiled to
// WebAssembly. It parses one CSV text (UTF-16LE code units, written by the
// JS driver at byte offset 64 of this module's linear memory) exactly the
// way csv-reader 1.0.0 reads it, and writes out field spans, record tokens
// and error codes. Field VALUES are spans of the original text, except
// fields that need a transform (doubled quotes, CR normalisation, dropped
// stray CRs, junk, unterminated quote) whose values are written as UTF-16
// into the values buffer. The JS driver slices the strings from those spans.
//
// The scanner is scalar on purpose: the SIMD chunk loops (v128) were
// removed after they corrupted values on the practice set. The cause was
// v128 loads on unaligned input offsets (a chunk can start at any u16
// position, so its byte address is not 16-byte aligned); the scalar wasm
// is still ~9x the baseline, so the SIMD is not worth the risk.
//
// Memory layout (all byte offsets, computed from the input length in u16):
//   [32, 64)   result header: 8 x u32 (at byte 32, not 0: a null pointer
//               at 0 is UB for the compiler and traps the header write)
//               [0] n_fields  [1] n_records  [2] n_errors
//               [3] fields_ptr [4] records_ptr [5] errors_ptr
//               [6] values_ptr (bytes) [7] values_len (u16 count)
//   [64, ...)  input (input_len u16)
//   fields:    (input_len+2) x 12 bytes: start u32, end u32, flags u32
//   records:   (input_len+2) x 36 bytes: line u32, row i32, first_field u32,
//              field_count u32, err_start u32, err_count u32,
//              width_err u32, width_payload u32, flags u32
//   errors:    (3*input_len+8) x 4 bytes: code u32
//   values:    (2*input_len+16) u16 of transformed field values
//
// Field flags: 1 = value lives in the values buffer (slice valuesStr).
// Record flags: 1 = skipped record (emit its errors only), 2 = header record.
// Error codes: 0 stray_cr, 1 unterminated_quote, 2 quote_junk.
// Width errors: 0 none, 1 short_row (payload = field count), 2 long_row.

#![no_std]
use core::arch::wasm32::*;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unreachable()
}

const E_STRAY_CR: u32 = 0;
const E_UNTERM: u32 = 1;
const E_JUNK: u32 = 2;

const W_NONE: u32 = 0;
const W_SHORT: u32 = 1;
const W_LONG: u32 = 2;

const F_SKIPPED: u32 = 1;
const F_HEADER: u32 = 2;

const FL_VALUES: u32 = 1;

const COMMA: u32 = 0;
const LINE_BREAK: u32 = 1;
const END: u32 = 2;

#[inline(always)]
fn align_up(x: u32, a: u32) -> u32 {
    (x + a - 1) & !(a - 1)
}

/// Total linear-memory bytes needed for an input of `input_len` u16 units.
#[no_mangle]
pub extern "C" fn needed_bytes(input_len: u32) -> u32 {
    let (_f, _r, _e, _v, total) = layout(input_len);
    total
}

fn layout(input_len: u32) -> (u32, u32, u32, u32, u32) {
    let input_bytes = input_len * 2;
    let fields_ptr = align_up(64 + input_bytes, 8);
    let records_ptr = fields_ptr + (input_len + 2) * 12;
    let errors_ptr = records_ptr + (input_len + 2) * 36;
    let values_ptr = errors_ptr + (3 * input_len + 8) * 4;
    let total = values_ptr + (2 * input_len + 16) * 2;
    (fields_ptr, records_ptr, errors_ptr, values_ptr, total)
}

#[inline(always)]
unsafe fn rd16(base: *const u16, i: u32) -> u16 {
    *base.add(i as usize)
}

#[inline(always)]
unsafe fn wr32(base: *mut u32, i: u32, v: u32) {
    *base.add(i as usize) = v;
}

#[inline(always)]
unsafe fn wr16(base: *mut u16, i: u32, v: u16) {
    *base.add(i as usize) = v;
}

/// Bitmask of lanes (8 u16s) equal to one of ',', LF, CR.
#[inline(always)]
unsafe fn mask_unquoted(v: v128) -> u32 {
    let m = v128_or(
        v128_or(i16x8_eq(v, i16x8_splat(44)), i16x8_eq(v, i16x8_splat(10))),
        i16x8_eq(v, i16x8_splat(13)),
    );
    i16x8_bitmask(m) as u32
}

/// Bitmask of lanes equal to '"'.
#[inline(always)]
unsafe fn mask_quote(v: v128) -> u32 {
    i16x8_bitmask(i16x8_eq(v, i16x8_splat(34))) as u32
}

struct Scan {
    inp: *const u16,
    len: u32,
    pos: u32,
    line: u32,
    // output cursors
    fields: *mut u32,
    records: *mut u32,
    errors: *mut u32,
    values: *mut u16,
    n_fields: u32,
    n_records: u32,
    n_errors: u32,
    vcur: u32, // next free u16 slot in the values buffer
    // record being read
    record_line: u32,
    record_row: i32,
    err_start: u32, // errors count at the start of this record
    // field scratch (set by the field readers)
    f_start: u32,
    f_end: u32,
    f_flags: u32,
    f_ended: u32,
    f_added: u32,
}

impl Scan {
    #[inline(always)]
    unsafe fn push_error(&mut self, code: u32) {
        wr32(self.errors, self.n_errors, code);
        self.n_errors += 1;
    }

    #[inline(always)]
    unsafe fn push_field(&mut self) {
        let o = self.n_fields * 3;
        wr32(self.fields, o, self.f_start);
        wr32(self.fields, o + 1, self.f_end);
        wr32(self.fields, o + 2, self.f_flags);
        self.n_fields += 1;
    }

    /// Copy input u16 units [s, e) into the values buffer at vcur.
    #[inline(always)]
    unsafe fn copy_values(&mut self, s: u32, e: u32) {
        if e > s {
            let n = (e - s) as usize;
            core::ptr::copy_nonoverlapping(self.inp.add(s as usize), self.values.add(self.vcur as usize), n);
            self.vcur += e - s;
        }
    }

    #[inline(always)]
    unsafe fn push_value_u16(&mut self, v: u16) {
        wr16(self.values, self.vcur, v);
        self.vcur += 1;
    }

    /// Read an unquoted field. `base_values` is true when the value must go
    /// to the values buffer (junk after a quote, or an unterminated quote
    /// whose '"' prefix the caller already wrote). Sets f_*.
    unsafe fn read_unquoted(&mut self, base_values: bool) {
        let fstart = self.pos;
        let mut span_start = self.pos;
        let mut transformed = base_values;
        let mut vstart = self.vcur;
        if transformed {
            vstart = self.vcur;
        }
        loop {
            if false && self.pos + 8 <= self.len {
                let v = v128_load((self.inp as *const u8).add((self.pos as usize) * 2) as *const v128);
                let m = mask_unquoted(v);
                if m == 0 {
                    self.pos += 8;
                    continue;
                }
                let b = m.trailing_zeros();
                let d = self.pos + b;
                let c = rd16(self.inp, d);
                if c == 44 {
                    // ','
                    self.finish_unquoted(fstart, span_start, d, transformed, vstart, COMMA);
                    self.pos = d + 1;
                    return;
                }
                if c == 10 {
                    // LF
                    self.finish_unquoted(fstart, span_start, d, transformed, vstart, LINE_BREAK);
                    self.pos = d + 1;
                    self.line += 1;
                    return;
                }
                if d + 1 < self.len && rd16(self.inp, d + 1) == 10 {
                    // CRLF
                    self.finish_unquoted(fstart, span_start, d, transformed, vstart, LINE_BREAK);
                    self.pos = d + 2;
                    self.line += 1;
                    return;
                }
                // Stray CR: dropped, error, the field goes on.
                self.push_error(E_STRAY_CR);
                if !transformed {
                    transformed = true;
                    vstart = self.vcur;
                }
                self.copy_values(span_start, d);
                self.pos = d + 1;
                span_start = self.pos;
                continue;
            }
            // Scalar tail (fewer than 8 units left).
            if self.pos >= self.len {
                self.finish_unquoted(fstart, span_start, self.len, transformed, vstart, END);
                return;
            }
            let c = rd16(self.inp, self.pos);
            if c == 44 {
                let end = self.pos;
                self.finish_unquoted(fstart, span_start, end, transformed, vstart, COMMA);
                self.pos = end + 1;
                return;
            }
            if c == 10 {
                let end = self.pos;
                self.finish_unquoted(fstart, span_start, end, transformed, vstart, LINE_BREAK);
                self.pos = end + 1;
                self.line += 1;
                return;
            }
            if c == 13 {
                if self.pos + 1 < self.len && rd16(self.inp, self.pos + 1) == 10 {
                    let end = self.pos;
                    self.finish_unquoted(fstart, span_start, end, transformed, vstart, LINE_BREAK);
                    self.pos = end + 2;
                    self.line += 1;
                    return;
                }
                self.push_error(E_STRAY_CR);
                if !transformed {
                    transformed = true;
                    vstart = self.vcur;
                }
                self.copy_values(span_start, self.pos);
                self.pos += 1;
                span_start = self.pos;
                continue;
            }
            self.pos += 1;
        }
    }

    #[inline(always)]
    unsafe fn finish_unquoted(&mut self, fstart: u32, span_start: u32, end: u32, transformed: bool, vstart: u32, ended: u32) {
        if transformed {
            self.copy_values(span_start, end);
            self.f_start = vstart;
            self.f_end = self.vcur;
            self.f_flags = FL_VALUES;
        } else {
            self.f_start = fstart;
            self.f_end = end;
            self.f_flags = 0;
        }
        self.f_ended = ended;
        // f_added: characters kept by this unquoted read (span lengths minus
        // dropped CRs). For the junk check the caller uses it: it is the
        // length of the value built here minus the base the caller wrote.
        self.f_added = self.f_end - self.f_start;
    }

    /// Scan the content [s, e) of a quoted field: returns (line_delta,
    /// has_cr). line_delta counts every LF and every CR not followed by LF.
    unsafe fn scan_quoted_content(&self, s: u32, e: u32) -> (u32, bool) {
        let mut lines = 0u32;
        let mut has_cr = false;
        let mut p = s;
        while false && p + 8 <= e {
            let v = v128_load((self.inp as *const u8).add((p as usize) * 2) as *const v128);
            let lf = i16x8_bitmask(i16x8_eq(v, i16x8_splat(10)));
            let cr = i16x8_bitmask(i16x8_eq(v, i16x8_splat(13)));
            lines += lf.count_ones();
            if cr != 0 {
                has_cr = true;
                // Lone CRs: a CR whose next unit is not LF. Check each set bit.
                let mut bits = cr;
                while bits != 0 {
                    let b = bits.trailing_zeros();
                    let d = p + b;
                    if d + 1 >= e || rd16(self.inp, d + 1) != 10 {
                        lines += 1;
                    }
                    bits &= bits - 1;
                }
            }
            p += 8;
        }
        while p < e {
            let c = rd16(self.inp, p);
            if c == 10 {
                lines += 1;
            } else if c == 13 {
                has_cr = true;
                if p + 1 >= e || rd16(self.inp, p + 1) != 10 {
                    lines += 1;
                }
            }
            p += 1;
        }
        (lines, has_cr)
    }

    /// Copy a quoted field's content [s, e) into the values buffer with the
    /// transforms applied: '""' becomes '"' and every CRLF / lone CR
    /// becomes LF. Scalar; only used for fields that need a transform.
    unsafe fn build_quoted_value(&mut self, s: u32, e: u32) {
        let mut p = s;
        while p < e {
            let c = rd16(self.inp, p);
            if c == 34 {
                // A doubled quote here (a lone quote cannot occur: e is the
                // closing quote, and any '"' inside is part of a pair).
                self.push_value_u16(34);
                p += 2;
                continue;
            }
            if c == 13 {
                self.push_value_u16(10);
                p += if p + 1 < e && rd16(self.inp, p + 1) == 10 { 2 } else { 1 };
                continue;
            }
            self.push_value_u16(c);
            p += 1;
        }
    }

    /// Read a quoted field whose opening quote is at pos. Sets f_*.
    unsafe fn read_quoted(&mut self) {
        let open = self.pos;
        let open_line = self.line;
        self.pos += 1;
        let mut has_double = false;
        let mut close: u32 = u32::MAX;
        // Scan for the closing quote.
        loop {
            if false && self.pos + 8 <= self.len {
                let v = v128_load((self.inp as *const u8).add((self.pos as usize) * 2) as *const v128);
                let m = mask_quote(v);
                if m == 0 {
                    self.pos += 8;
                    continue;
                }
                // Walk the quote bits in this chunk, lowest first.
                let mut bits = m;
                let mut found_close = false;
                while bits != 0 {
                    let b = bits.trailing_zeros();
                    let q = self.pos + b;
                    if q + 1 < self.len && rd16(self.inp, q + 1) == 34 {
                        has_double = true;
                        self.pos = q + 2;
                        bits &= bits - 1;
                        // The rest of this chunk may hold more quotes; keep
                        // walking them, but the chunk is now stale (pos
                        // moved past q+2): recompute from the new pos.
                        // Simplest correct step: restart the chunk scan.
                        found_close = false;
                        break;
                    } else {
                        close = q;
                        found_close = true;
                        break;
                    }
                }
                if found_close {
                    break;
                }
                // Either the chunk had no quotes (handled above) or we
                // consumed a doubled quote and must rescan from the new pos.
                continue;
            }
            // Scalar tail.
            if self.pos >= self.len {
                break;
            }
            let c = rd16(self.inp, self.pos);
            if c == 34 {
                if self.pos + 1 < self.len && rd16(self.inp, self.pos + 1) == 34 {
                    has_double = true;
                    self.pos += 2;
                    continue;
                }
                close = self.pos;
                break;
            }
            self.pos += 1;
        }

        if close == u32::MAX {
            // The quote never closes: literal, the rest read again as unquoted.
            self.pos = open + 1;
            self.line = open_line;
            self.push_error(E_UNTERM);
            let vstart = self.vcur;
            self.push_value_u16(34); // the opening quote, as a literal
            self.read_unquoted(true);
            // read_unquoted set f_start/f_end from vstart..vcur; fix the
            // start (it recorded vcur before the '"' was written... no:
            // read_unquoted(true) uses vcur at entry as vstart, which is
            // AFTER the '"'. Correct it to include the quote.
            self.f_start = vstart;
            self.f_flags = FL_VALUES;
            return;
        }

        let content_s = open + 1;
        let content_e = close;
        let (lines, has_cr) = self.scan_quoted_content(content_s, content_e);
        self.line += lines;

        // Position just past the closing quote.
        let after = close + 1;
        if after >= self.len {
            // Field ends at the end of the text.
            self.set_quoted_value(content_s, content_e, has_double, has_cr);
            self.pos = after;
            self.f_ended = END;
            return;
        }
        let c = rd16(self.inp, after);
        if c == 44 {
            self.set_quoted_value(content_s, content_e, has_double, has_cr);
            self.pos = after + 1;
            self.f_ended = COMMA;
            return;
        }
        if c == 10 {
            self.set_quoted_value(content_s, content_e, has_double, has_cr);
            self.pos = after + 1;
            self.line += 1;
            self.f_ended = LINE_BREAK;
            return;
        }
        if c == 13 && after + 1 < self.len && rd16(self.inp, after + 1) == 10 {
            self.set_quoted_value(content_s, content_e, has_double, has_cr);
            self.pos = after + 2;
            self.line += 1;
            self.f_ended = LINE_BREAK;
            return;
        }
        // Junk after the closing quote, appended as it stands.
        // The value must go to the values buffer (content + junk).
        let vstart = self.vcur;
        if has_double || has_cr {
            self.build_quoted_value(content_s, content_e);
        } else {
            self.copy_values(content_s, content_e);
        }
        self.pos = after;
        let junk_start_v = self.vcur;
        self.read_unquoted(true);
        // read_unquoted(true) set f_start to vcur at its entry (junk_start_v)
        // and f_end to vcur; the value is [vstart, vcur).
        self.f_start = vstart;
        self.f_end = self.vcur;
        self.f_flags = FL_VALUES;
        // f_added is the junk length the unquoted read kept (it measured
        // from its own start); recompute: junk kept = vcur - junk_start_v.
        let junk_added = self.vcur - junk_start_v;
        self.f_added = junk_added;
        if junk_added > 0 {
            self.push_error(E_JUNK);
        }
    }

    /// Set f_start/f_end/f_flags for a quoted field that ended cleanly
    /// (no junk): plain slice when nothing needs a transform, else the
    /// values buffer.
    #[inline(always)]
    unsafe fn set_quoted_value(&mut self, s: u32, e: u32, has_double: bool, has_cr: bool) {
        if !has_double && !has_cr {
            self.f_start = s;
            self.f_end = e;
            self.f_flags = 0;
        } else {
            let vstart = self.vcur;
            self.build_quoted_value(s, e);
            self.f_start = vstart;
            self.f_end = self.vcur;
            self.f_flags = FL_VALUES;
        }
        self.f_added = 0;
    }
}

/// Parse the input at byte offset 64 (input_len u16 units). `header` is 1
/// when the first record is the header. Returns 1 on success.
#[no_mangle]
pub extern "C" fn parse(input_len: u32, header: u32) -> u32 {
    unsafe { parse_inner(input_len, header) }
}

unsafe fn parse_inner(input_len: u32, header: u32) -> u32 {
    let (fields_ptr, records_ptr, errors_ptr, values_ptr, _total) = layout(input_len);
    let inp = 64 as *const u16;
    let hdr = 32 as *mut u32;

    let mut sc = Scan {
        inp,
        len: input_len,
        pos: 0,
        line: 1,
        fields: fields_ptr as *mut u32,
        records: records_ptr as *mut u32,
        errors: errors_ptr as *mut u32,
        values: values_ptr as *mut u16,
        n_fields: 0,
        n_records: 0,
        n_errors: 0,
        vcur: 0,
        record_line: 1,
        record_row: 0,
        err_start: 0,
        f_start: 0,
        f_end: 0,
        f_flags: 0,
        f_ended: END,
        f_added: 0,
    };

    if input_len > 0 && rd16(inp, 0) == 0xfeff {
        sc.pos = 1;
    }

    let header_b = header != 0;
    let mut columns_set = false;
    let mut width: i64 = -1;
    let mut rows_len: u32 = 0; // records kept in rows so far

    while true {
        // Lines with no characters are no records.
        while sc.pos < sc.len {
            let c = rd16(inp, sc.pos);
            if c == 10 {
                sc.pos += 1;
                sc.line += 1;
            } else if c == 13 && sc.pos + 1 < sc.len && rd16(inp, sc.pos + 1) == 10 {
                sc.pos += 2;
                sc.line += 1;
            } else {
                break;
            }
        }
        if sc.pos >= sc.len {
            break;
        }

        sc.record_line = sc.line;
        sc.record_row = if header_b && !columns_set { -1 } else { rows_len as i32 };
        sc.err_start = sc.n_errors;
        let rec_first_field = sc.n_fields;

        let mut quoted = false;
        loop {
            quoted = sc.pos < sc.len && rd16(inp, sc.pos) == 34;
            if quoted {
                sc.read_quoted();
            } else {
                sc.read_unquoted(false);
            }
            sc.push_field();
            if sc.f_ended != COMMA {
                break;
            }
        }
        let field_count = sc.n_fields - rec_first_field;

        // A line that held only stray CRs is left with no characters: no
        // record (its stray_cr errors stay). The single field's value is
        // empty exactly when its span is empty (plain slice or values
        // buffer alike).
        let single_empty = field_count == 1 && !quoted && sc.f_start == sc.f_end;
        if single_empty {
            emit_record(&mut sc, rec_first_field, field_count, W_NONE, 0, F_SKIPPED);
            continue;
        }

        if width < 0 {
            width = field_count as i64;
        }
        if header_b && !columns_set {
            columns_set = true;
            emit_record(&mut sc, rec_first_field, field_count, W_NONE, 0, F_HEADER);
            continue;
        }
        if (field_count as i64) < width {
            emit_record(
                &mut sc,
                rec_first_field,
                field_count,
                W_SHORT,
                field_count,
                0,
            );
        } else if (field_count as i64) > width {
            emit_record(
                &mut sc,
                rec_first_field,
                field_count,
                W_LONG,
                0,
                0,
            );
        } else {
            emit_record(&mut sc, rec_first_field, field_count, W_NONE, 0, 0);
        }
        rows_len += 1;
    }

    // Header for the JS driver.
    wr32(hdr, 0, sc.n_fields);
    wr32(hdr, 1, sc.n_records);
    wr32(hdr, 2, sc.n_errors);
    wr32(hdr, 3, fields_ptr);
    wr32(hdr, 4, records_ptr);
    wr32(hdr, 5, errors_ptr);
    wr32(hdr, 6, values_ptr);
    wr32(hdr, 7, sc.vcur);
    1
}

#[inline(always)]
unsafe fn emit_record(sc: &mut Scan, first_field: u32, field_count: u32, width_err: u32, width_payload: u32, flags: u32) {
    let o = sc.n_records * 9;
    wr32(sc.records, o, sc.record_line);
    wr32(sc.records, o + 1, sc.record_row as u32);
    wr32(sc.records, o + 2, first_field);
    wr32(sc.records, o + 3, field_count);
    wr32(sc.records, o + 4, sc.err_start);
    wr32(sc.records, o + 5, sc.n_errors - sc.err_start);
    wr32(sc.records, o + 6, width_err);
    wr32(sc.records, o + 7, width_payload);
    wr32(sc.records, o + 8, flags);
    sc.n_records += 1;
}
