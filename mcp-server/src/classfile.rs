// Just enough of the class-file format to answer one question: does the bytecode this JVM is running
// come from the build sitting on disk? (DISC-7, #59.)
//
// **Why a parser is here at all**, in a project whose whole point is asking the JVM rather than
// guessing from local files. `debug.source` (DISC-3, #31) already settles drift at *file* granularity —
// a class reporting `Order.java` in a tree where that file was renamed is the answer. It cannot settle
// the case that actually fires in a redeploy loop: **same class, same `Order.java`, older bytecode.**
// `SourceFile` is a compile-time string and is identical across every build of the file. Comparing what
// the JVM has against what the compiler just produced needs both sides read, and only one of them is
// reachable over JDWP.
//
// **What is read, and what is deliberately not.** Only the constant pool (to resolve names), the method
// table, and each method's `LineNumberTable`. Not the bytecode, not the stack map, not the field table.
// The comparison this feeds is a line-table comparison, chosen over `Method.Bytecodes` because the line
// table is already implemented on the JDWP side and catches the case that hurts most — line numbers
// shifted, so a stop point at `:412` now means something else. It is blind to an edit that moves no
// line, and `check_stale`'s reply says so rather than overclaiming.
//
// Everything here is bounds-checked against a hostile file: the class root is a directory an operator
// named, and a truncated `.class` left by a killed build is the ordinary case, not the exotic one.

/// One method as the class file declares it.
#[derive(Debug, Clone)]
pub struct ClassFileMethod {
    pub name: String,
    /// The JVM descriptor, e.g. `(Ljava/lang/String;)I`. Kept unrendered because it is being *compared*
    /// with what JDWP reports, and JDWP reports descriptors.
    pub descriptor: String,
    /// `(start_pc, line)` pairs from the `LineNumberTable`, in file order.
    pub lines: Vec<(u64, i32)>,
    /// Whether the method has a `Code` attribute at all. `false` for abstract and native methods, which
    /// have no body to compare and must not be reported as drift.
    pub has_code: bool,
    /// Whether the `Code` attribute carried a `LineNumberTable`. A method compiled `-g:none` has code
    /// and no lines, which is a third state and not the same as either of the above.
    pub has_line_table: bool,
    /// The method's bytecode, for comparison against `Method.Bytecodes` (DISC-9, #63). Empty when the
    /// method has no `Code` attribute at all.
    ///
    /// This is the evidence that survives `-g:none`: a stripped build has code and no lines, so it is
    /// exactly where a line-table comparison must answer "cannot tell" and this one can answer.
    pub code: Vec<u8>,
}

/// The parts of a `.class` file the staleness check compares.
#[derive(Debug, Clone)]
pub struct ClassFile {
    /// The class this file declares, dotted (`com.example.Order`). Compared against the class being
    /// checked, because a root that resolves to the wrong file is a likelier mistake than drift.
    pub this_class: String,
    pub methods: Vec<ClassFileMethod>,
}

/// A bounds-checked reader over the file's bytes.
///
/// Not `bytes::Buf`, whose getters panic on a short buffer — the input here is a file that may have been
/// truncated by a killed build, and `panic` is not an answer this server may give.
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("class file: length overflow")?;
        let slice = self.data.get(self.at..end).ok_or_else(|| {
            format!(
                "class file is truncated: wanted {n} byte(s) at offset {}, file is {} byte(s)",
                self.at,
                self.data.len()
            )
        })?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(*self.take(1)?.first().ok_or("class file: unreachable short read")?)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from(*b.first().ok_or("short")?) << 8 | u16::from(*b.get(1).ok_or("short")?))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from(self.u16()?) << 16 | u32::from(self.u16()?))
    }

    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.take(n).map(|_| ())
    }
}

/// One constant-pool entry, in the only two shapes this parser needs to resolve.
enum Constant {
    Utf8(String),
    /// A `CONSTANT_Class`, holding the pool index of its name.
    Class(u16),
    /// Everything else, including the second (unusable) slot of a long or double.
    Other,
}

/// Parse the parts of `bytes` that a staleness comparison needs.
///
/// # Errors
/// Returns a message naming what could not be read. Every failure is a fact about the file — truncated,
/// not a class file, a constant-pool tag this parser does not know — and the caller reports it as such
/// rather than as "not stale".
pub fn parse(bytes: &[u8]) -> Result<ClassFile, String> {
    let mut c = Cursor::new(bytes);
    if c.u32()? != 0xCAFE_BABE {
        return Err("not a class file: it does not start with 0xCAFEBABE".to_string());
    }
    // Version: read past rather than kept. It is the obvious thing to report, and nothing here acts on
    // it — a file this JVM cannot load is refused by the JVM, with UNSUPPORTED_VERSION, which is a
    // better answer than one derived from a number we compared ourselves.
    c.skip(4)?; // minor, major
    let pool = parse_constant_pool(&mut c)?;

    let _access_flags = c.u16()?;
    let this_class = c.u16()?;
    let _super_class = c.u16()?;
    let interfaces = c.u16()? as usize;
    c.skip(interfaces * 2)?;

    skip_members(&mut c)?; // fields
    let methods = parse_methods(&mut c, &pool)?;

    Ok(ClassFile { this_class: class_name(&pool, this_class)?, methods })
}

fn parse_constant_pool(c: &mut Cursor) -> Result<Vec<Constant>, String> {
    let count = c.u16()?;
    // Index 0 is unused by the format; keeping a placeholder makes every later lookup 1-based without
    // arithmetic at each call site.
    let mut pool = vec![Constant::Other];
    let mut i = 1;
    while i < count {
        let tag = c.u8()?;
        let entry = read_constant(c, tag, i)?;
        // A long or double "takes up two entries", says the spec, and the second is unusable. Getting
        // this wrong shifts every later index and produces confident nonsense rather than an error,
        // which is why it is here in the walk rather than buried in the reader.
        let slots = if matches!(tag, 5 | 6) { 2 } else { 1 };
        pool.push(entry);
        if slots == 2 {
            pool.push(Constant::Other);
        }
        i += slots;
    }
    Ok(pool)
}

/// One constant-pool entry, by tag. `at` names the entry for the error message only.
fn read_constant(c: &mut Cursor, tag: u8, at: u16) -> Result<Constant, String> {
    // Widths by tag, from the spec's table. Everything but Utf8 and Class is skipped: this parser
    // resolves names and nothing else, so decoding a `Methodref` would be weight nothing reads.
    const SKIP: &[(u8, usize)] = &[
        (8, 2),
        (16, 2),
        (19, 2),
        (20, 2),
        (15, 3),
        (3, 4),
        (4, 4),
        (9, 4),
        (10, 4),
        (11, 4),
        (12, 4),
        (17, 4),
        (18, 4),
        (5, 8),
        (6, 8),
    ];
    match tag {
        // A `CONSTANT_Utf8` is *modified* UTF-8, which differs from UTF-8 for the NUL character and for
        // characters outside the BMP. Every name this parser compares is a Java identifier or a
        // descriptor, where the two encodings coincide, so a lossy decode is right here and a decoder
        // for the general case would be unused weight.
        1 => {
            let len = c.u16()? as usize;
            Ok(Constant::Utf8(String::from_utf8_lossy(c.take(len)?).into_owned()))
        }
        7 => Ok(Constant::Class(c.u16()?)),
        _ => match SKIP.iter().find(|(t, _)| *t == tag) {
            Some((_, width)) => {
                c.skip(*width)?;
                Ok(Constant::Other)
            }
            None => Err(format!("class file: unknown constant pool tag {tag} at entry {at}")),
        },
    }
}

/// Skip a `field_info` or `method_info` table wholesale — used for fields, which this comparison does
/// not read.
fn skip_members(c: &mut Cursor) -> Result<(), String> {
    let count = c.u16()?;
    for _ in 0..count {
        c.skip(6)?; // access_flags, name_index, descriptor_index
        let attrs = c.u16()?;
        for _ in 0..attrs {
            c.skip(2)?; // attribute_name_index
            let len = c.u32()? as usize;
            c.skip(len)?;
        }
    }
    Ok(())
}

fn parse_methods(c: &mut Cursor, pool: &[Constant]) -> Result<Vec<ClassFileMethod>, String> {
    let count = c.u16()?;
    let mut methods = Vec::with_capacity(count as usize);
    for _ in 0..count {
        methods.push(parse_one_method(c, pool)?);
    }
    Ok(methods)
}

/// One `method_info`: its name, its descriptor, and whatever its `Code` attribute says about lines.
fn parse_one_method(c: &mut Cursor, pool: &[Constant]) -> Result<ClassFileMethod, String> {
    let _access = c.u16()?;
    let name = utf8(pool, c.u16()?)?;
    let descriptor = utf8(pool, c.u16()?)?;
    let attrs = c.u16()?;
    let mut lines = Vec::new();
    let mut has_code = false;
    let mut has_line_table = false;
    let mut code = Vec::new();
    for _ in 0..attrs {
        let attr_name = utf8(pool, c.u16()?)?;
        let len = c.u32()? as usize;
        if attr_name == "Code" {
            has_code = true;
            let body = c.take(len)?;
            let attr = parse_code_body(body, pool)?;
            has_line_table = attr.has_line_table;
            lines = attr.lines;
            code = attr.code;
        } else {
            c.skip(len)?;
        }
    }
    Ok(ClassFileMethod { name, descriptor, lines, has_code, has_line_table, code })
}

/// What a `Code` attribute yields to the staleness comparison.
struct CodeAttribute {
    lines: Vec<(u64, i32)>,
    /// Whether a `LineNumberTable` was present at all — `-g:none` has code and no lines, a third state.
    has_line_table: bool,
    code: Vec<u8>,
}

/// Pull the bytecode and the `LineNumberTable` out of a `Code` attribute's body.
///
/// The code array was already being measured in order to skip past it, so keeping it costs one copy and
/// no extra parsing (DISC-9).
fn parse_code_body(body: &[u8], pool: &[Constant]) -> Result<CodeAttribute, String> {
    let mut c = Cursor::new(body);
    c.skip(4)?; // max_stack, max_locals
    let code_len = c.u32()? as usize;
    let code = c.take(code_len)?.to_vec();
    let exceptions = c.u16()? as usize;
    c.skip(exceptions * 8)?;

    let attrs = c.u16()?;
    for _ in 0..attrs {
        let name = utf8(pool, c.u16()?)?;
        let len = c.u32()? as usize;
        if name == "LineNumberTable" {
            let lines = read_line_number_table(c.take(len)?)?;
            return Ok(CodeAttribute { lines, has_line_table: true, code });
        }
        c.skip(len)?;
    }
    Ok(CodeAttribute { lines: Vec::new(), has_line_table: false, code })
}

/// The `(start_pc, line)` pairs inside a `LineNumberTable` attribute's body.
fn read_line_number_table(body: &[u8]) -> Result<Vec<(u64, i32)>, String> {
    let mut table = Cursor::new(body);
    let entries = table.u16()?;
    let mut lines = Vec::with_capacity(entries as usize);
    for _ in 0..entries {
        let start_pc = u64::from(table.u16()?);
        let line = i32::from(table.u16()?);
        lines.push((start_pc, line));
    }
    Ok(lines)
}

fn utf8(pool: &[Constant], index: u16) -> Result<String, String> {
    match pool.get(index as usize) {
        Some(Constant::Utf8(s)) => Ok(s.clone()),
        _ => Err(format!("class file: constant pool entry {index} is not a UTF-8 constant")),
    }
}

fn class_name(pool: &[Constant], index: u16) -> Result<String, String> {
    match pool.get(index as usize) {
        Some(Constant::Class(name_index)) => Ok(utf8(pool, *name_index)?.replace('/', ".")),
        _ => Err(format!("class file: constant pool entry {index} is not a class constant")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every failure has to be a *statement about the file*, because the caller turns each into different
    // advice: a truncated file is a failed build, a wrong magic is a wrong path.
    #[test]
    fn a_file_that_is_not_a_class_file_is_named_as_such() {
        assert!(parse(b"public class Order {}").unwrap_err().contains("0xCAFEBABE"));
        assert!(parse(&[]).unwrap_err().contains("truncated"));
        // Valid magic, nothing after it: the commonest shape of a build killed mid-write.
        assert!(parse(&[0xCA, 0xFE, 0xBA, 0xBE]).unwrap_err().contains("truncated"));
    }

    // The long/double two-slot rule. Getting it wrong does not fail — it shifts every subsequent pool
    // index and yields confident nonsense, so it is asserted directly rather than trusted to a
    // round-trip through a real class file.
    #[test]
    fn a_long_constant_consumes_two_pool_slots() {
        // pool_count = 4 → entries 1..3: a Long (taking 1 and 2), then a Utf8 at 3.
        let mut bytes: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 65, 0, 4];
        bytes.push(5); // CONSTANT_Long
        bytes.extend_from_slice(&7i64.to_be_bytes());
        bytes.push(1); // CONSTANT_Utf8
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.extend_from_slice(b"Hey");

        let mut c = Cursor::new(&bytes);
        assert_eq!(c.u32().unwrap(), 0xCAFE_BABE);
        c.skip(4).unwrap();
        let pool = parse_constant_pool(&mut c).unwrap();
        // Index 3 must still be the Utf8: if the second Long slot were missing it would sit at 2.
        assert_eq!(utf8(&pool, 3).unwrap(), "Hey");
        assert!(utf8(&pool, 2).is_err(), "the second slot of a Long is unusable, not the next constant");
    }
}
