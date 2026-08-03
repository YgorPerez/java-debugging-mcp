//! Rendering JVM **generic** signatures as Java source (DISC-12, [#95]).
//!
//! [#95]: https://github.com/YgorPerez/java-debugging-mcp/issues/95
//!
//! A generic signature is a *different grammar* from the plain descriptor `decode_signature` reads, not a
//! superset of it: it has type arguments, wildcards, type variables, nested types that each carry their own
//! arguments, type parameters with bounds, and a `throws` clause. JVMS 4.7.9.1 is the grammar; this module
//! is a full parser for it, which is the decision #95 recorded — the alternative it offered was printing the
//! raw signature verbatim, and handing a caller `Ljava/util/List<Lcom/x/Reserva;>;` to read would have made
//! the tool's output *less* like Java rather than more.
//!
//! **Everything here returns `Option` and the caller falls back to the raw descriptor.** A generic signature
//! is an optional class-file attribute: absent for code compiled without it, absent after erasure in some
//! synthetic members, absent on arrays of type variables. The JDWP generic commands answer with an **empty
//! string** in that case rather than an error, so a naive implementation renders a blank type. Nothing here
//! can produce one: an empty or unparseable signature is `None`, and `None` means "use what you had".
//!
//! It is also deliberately *total* — no panics, no indexing — because the input is attacker-adjacent in the
//! only sense that matters here: it comes from a class file this tool did not compile, and a malformed one
//! must degrade to the raw descriptor rather than take a session down.

/// A parsed method signature, in the pieces a declaration is written from.
#[derive(Debug, PartialEq, Eq)]
pub struct MethodParts {
    /// `<T, U extends Number>` — empty for a method that declares none.
    pub type_params: Vec<String>,
    /// Parameter types, in order, already rendered as Java.
    pub params: Vec<String>,
    /// The return type, `void` included.
    pub ret: String,
    /// `throws` types, which a generic signature carries only when one of them is a type variable.
    pub throws: Vec<String>,
}

/// A cursor over the signature's bytes.
///
/// Bytes rather than chars: every byte the grammar cares about is ASCII, and an identifier is copied out
/// wholesale rather than inspected, so UTF-8 in a class name passes through untouched.
struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    const fn new(s: &'a str) -> Self {
        Self { s: s.as_bytes(), i: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    const fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }

    /// Copy out bytes up to (not including) any of `stop`. `None` if that would be empty, which is what
    /// makes a malformed signature fail rather than yield an empty type name.
    fn ident(&mut self, stop: &[u8]) -> Option<String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if stop.contains(&c) {
                break;
            }
            self.i += 1;
        }
        // `get` rather than a slice: `start <= self.i <= len` holds by construction here, but this module
        // is deliberately total — a malformed signature must degrade to the raw descriptor, never panic.
        let bytes = self.s.get(start..self.i)?;
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The Java name of a base-type descriptor character.
const fn base_type(c: u8) -> Option<&'static str> {
    Some(match c {
        b'B' => "byte",
        b'C' => "char",
        b'D' => "double",
        b'F' => "float",
        b'I' => "int",
        b'J' => "long",
        b'S' => "short",
        b'Z' => "boolean",
        b'V' => "void",
        _ => return None,
    })
}

/// Render a JVMS 4.7.9.1 **type** signature (a field's, a local's, an array element's) as Java source.
///
/// `None` for an empty, malformed, or trailing-garbage signature — the caller then uses the plain
/// descriptor it already had.
#[must_use]
pub fn render_type(sig: &str) -> Option<String> {
    let mut c = Cursor::new(sig);
    let out = type_sig(&mut c)?;
    // Trailing bytes mean this was not the signature it claimed to be. Refusing is the point: a partial
    // parse that dropped the tail would render a *plausible* wrong type, which is worse than none (#95).
    c.at_end().then_some(out)
}

/// Render a method's generic signature into its pieces.
///
/// `None` on anything that does not parse cleanly, including a type signature handed here by mistake.
#[must_use]
pub fn render_method(sig: &str) -> Option<MethodParts> {
    let mut c = Cursor::new(sig);
    let type_params = if c.peek() == Some(b'<') { type_parameters(&mut c)? } else { Vec::new() };
    if !c.eat(b'(') {
        return None;
    }
    let mut params = Vec::new();
    while c.peek().is_some_and(|b| b != b')') {
        params.push(type_sig(&mut c)?);
    }
    if !c.eat(b')') {
        return None;
    }
    let ret = type_sig(&mut c)?;
    let mut throws = Vec::new();
    while c.eat(b'^') {
        throws.push(type_sig(&mut c)?);
    }
    c.at_end().then_some(MethodParts { type_params, params, ret, throws })
}

/// `JavaTypeSignature`: an array, a class type, a type variable, or a base type.
fn type_sig(c: &mut Cursor) -> Option<String> {
    let mut dims = 0usize;
    while c.eat(b'[') {
        dims += 1;
        // A signature that is nothing but brackets is malformed, and the loop must not run away on one.
        if dims > 255 {
            return None;
        }
    }
    let base = match c.peek()? {
        b'L' => class_type(c)?,
        b'T' => {
            c.bump();
            let name = c.ident(b";")?;
            if !c.eat(b';') {
                return None;
            }
            name
        }
        other => {
            let name = base_type(other)?;
            c.bump();
            name.to_string()
        }
    };
    Some(format!("{base}{}", "[]".repeat(dims)))
}

/// `ClassTypeSignature`: `L` package/Simple\[<args>] { `.` Simple\[<args>] } `;`
///
/// The package separator becomes `.` unconditionally here, unlike `decode_signature`'s deliberate refusal
/// to rewrite the `/` in a lambda's generated name (SIG-1, #46). That is safe rather than an oversight: a
/// generated class has no `Signature` attribute, so no lambda name can reach this parser.
fn class_type(c: &mut Cursor) -> Option<String> {
    if !c.eat(b'L') {
        return None;
    }
    let mut out = String::new();
    loop {
        let name = c.ident(b"<;.")?;
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&name.replace('/', "."));
        if c.peek() == Some(b'<') {
            out.push_str(&type_arguments(c)?);
        }
        // A `.` starts a nested type, which carries its own arguments; a `;` ends the whole thing.
        if c.eat(b';') {
            return Some(out);
        }
        if !c.eat(b'.') {
            return None;
        }
    }
}

/// `TypeArguments`: `<` one-or-more `TypeArgument` `>`, rendered including the angle brackets.
fn type_arguments(c: &mut Cursor) -> Option<String> {
    if !c.eat(b'<') {
        return None;
    }
    let mut args: Vec<String> = Vec::new();
    loop {
        match c.peek()? {
            b'>' => break,
            // An unbounded wildcard. `*` is the whole argument — there is no type after it.
            b'*' => {
                c.bump();
                args.push("?".to_string());
            }
            b'+' => {
                c.bump();
                args.push(format!("? extends {}", type_sig(c)?));
            }
            b'-' => {
                c.bump();
                args.push(format!("? super {}", type_sig(c)?));
            }
            _ => args.push(type_sig(c)?),
        }
    }
    if !c.eat(b'>') || args.is_empty() {
        return None;
    }
    Some(format!("<{}>", args.join(", ")))
}

/// `TypeParameters`: `<` one-or-more `Identifier : [ClassBound] {: InterfaceBound}` `>`.
///
/// Rendered the way Java declares them, with the universal bound left off: `<T>` rather than
/// `<T extends java.lang.Object>`, which is what the class file always says and what nobody writes.
fn type_parameters(c: &mut Cursor) -> Option<Vec<String>> {
    if !c.eat(b'<') {
        return None;
    }
    let mut params = Vec::new();
    // Hoisted and cleared per parameter rather than allocated per parameter: a class with several bounded
    // type parameters is common in this stack's DTO hierarchies.
    let mut bounds: Vec<String> = Vec::new();
    while c.peek().is_some_and(|b| b != b'>') {
        let name = c.ident(b":")?;
        bounds.clear();
        // At least one `:` is required; a missing class bound (an interface-only parameter) shows as `::`.
        if !c.eat(b':') {
            return None;
        }
        if c.peek().is_some_and(|b| b != b':' && b != b'>') {
            bounds.push(type_sig(c)?);
        }
        while c.eat(b':') {
            bounds.push(type_sig(c)?);
        }
        bounds.retain(|b| b != "java.lang.Object");
        params.push(if bounds.is_empty() { name } else { format!("{name} extends {}", bounds.join(" & ")) });
    }
    if !c.eat(b'>') || params.is_empty() {
        return None;
    }
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The everyday cases, and the one the issue calls the worst found: a two-level map in `integraWS`
    /// that reads as nothing navigable when rendered raw.
    #[test]
    fn nested_type_arguments_render_as_java_source() {
        assert_eq!(
            render_type("Ljava/util/List<Ljava/lang/String;>;").as_deref(),
            Some("java.util.List<java.lang.String>")
        );
        assert_eq!(
            render_type("Ljava/util/Map<Ljava/lang/Integer;Ljava/util/List<Lcom/x/Widget;>;>;").as_deref(),
            Some("java.util.Map<java.lang.Integer, java.util.List<com.x.Widget>>")
        );
        assert_eq!(
            render_type(
                "Ljava/util/Map<Ljava/lang/Integer;Ljava/util/Map<Lcom/x/WSIntegradorEnum;Ljava/util/LinkedList<Lcom/x/WSSessao;>;>;>;"
            )
            .as_deref(),
            Some(
                "java.util.Map<java.lang.Integer, java.util.Map<com.x.WSIntegradorEnum, java.util.LinkedList<com.x.WSSessao>>>"
            )
        );
    }

    /// Wildcards, the acceptance criterion that says they must not be mangled.
    #[test]
    fn wildcards_render_the_way_java_writes_them() {
        assert_eq!(
            render_type("Ljava/util/Map<Ljava/lang/String;+Ljava/lang/Number;>;").as_deref(),
            Some("java.util.Map<java.lang.String, ? extends java.lang.Number>")
        );
        assert_eq!(
            render_type("Ljava/util/List<-Ljava/lang/Integer;>;").as_deref(),
            Some("java.util.List<? super java.lang.Integer>")
        );
        assert_eq!(
            render_type("Ljava/util/List<*>;").as_deref(),
            Some("java.util.List<?>"),
            "an unbounded wildcard is the whole argument — there is no type after the `*`"
        );
    }

    /// Type variables, arrays of them, and arrays of parameterised types.
    #[test]
    fn type_variables_and_arrays() {
        assert_eq!(render_type("TT;").as_deref(), Some("T"));
        assert_eq!(render_type("[TT;").as_deref(), Some("T[]"));
        assert_eq!(render_type("[[Ljava/util/List<TE;>;").as_deref(), Some("java.util.List<E>[][]"));
        assert_eq!(render_type("[I").as_deref(), Some("int[]"), "a base type still parses here");
    }

    /// A nested type carries its own arguments, and Java writes both.
    #[test]
    fn a_nested_type_keeps_the_outers_arguments() {
        assert_eq!(
            render_type("Lcom/x/Outer<Ljava/lang/String;>.Inner<Ljava/lang/Integer;>;").as_deref(),
            Some("com.x.Outer<java.lang.String>.Inner<java.lang.Integer>")
        );
        assert_eq!(
            render_type("Lcom/x/Outer.Inner;").as_deref(),
            Some("com.x.Outer.Inner"),
            "and a nested type with no arguments at all still parses"
        );
    }

    /// Methods: type parameters, parameters, return, and the `throws` a generic signature carries.
    #[test]
    fn method_signatures_split_into_the_pieces_a_declaration_needs() {
        let plain =
            render_method("(Ljava/util/List<Ljava/lang/String;>;I)Ljava/util/Map<Ljava/lang/String;TT;>;")
                .expect("a plain generic method signature must parse");
        assert!(plain.type_params.is_empty());
        assert_eq!(plain.params, vec!["java.util.List<java.lang.String>".to_string(), "int".to_string()]);
        assert_eq!(plain.ret, "java.util.Map<java.lang.String, T>");
        assert!(plain.throws.is_empty());

        let generic = render_method("<T:Ljava/lang/Object;>(TT;)TT;").expect("a generic method must parse");
        assert_eq!(
            generic.type_params,
            vec!["T".to_string()],
            "the universal bound is left off, as Java leaves it"
        );
        assert_eq!(generic.params, vec!["T".to_string()]);
        assert_eq!(generic.ret, "T");

        let bounded = render_method("<T:Ljava/lang/Number;:Ljava/lang/Comparable<TT;>;>(TT;)V")
            .expect("an intersection bound must parse");
        assert_eq!(
            bounded.type_params,
            vec!["T extends java.lang.Number & java.lang.Comparable<T>".to_string()]
        );
        assert_eq!(bounded.ret, "void");

        let thrown = render_method("<E:Ljava/lang/Throwable;>()V^TE;").expect("a generic throws must parse");
        assert_eq!(thrown.throws, vec!["E".to_string()]);

        let iface_only = render_method("<T::Ljava/lang/Comparable<TT;>;>(TT;)V")
            .expect("an interface-only bound (`::`) must parse");
        assert_eq!(iface_only.type_params, vec!["T extends java.lang.Comparable<T>".to_string()]);
    }

    /// **The whole design risk, and the reason every function here returns `Option`.** A generic signature
    /// is optional in the class file, and the JDWP generic commands answer with an EMPTY STRING rather than
    /// an error when there is none — so anything that cannot be rendered has to say so, or the tool prints
    /// a blank type where it used to print a correct raw one.
    #[test]
    fn nothing_unparseable_ever_renders_as_a_type() {
        for bad in [
            "",                                    // no attribute — the common case, by a wide margin
            "Ljava/util/List<Ljava/lang/String;>", // unterminated class type
            "Ljava/util/List<;>;",                 // an argument that is not a type
            "Ljava/util/List<>;",                  // empty argument list
            "L;",                                  // no name
            "T;",                                  // no type-variable name
            "Q",                                   // not a type character at all
            "Ljava/lang/String;X",                 // trailing garbage: a partial parse would look plausible
            "[",                                   // brackets and nothing else
            "*",                                   // a wildcard outside an argument list
        ] {
            assert!(render_type(bad).is_none(), "'{bad}' must not render as a type, but it did");
        }
        for bad in [
            "",
            "(I)",                // no return type
            "Ljava/lang/String;", // a TYPE signature, handed here by mistake
            "(I)VX",              // trailing garbage
            "<>(I)V",             // empty type parameters
            "<T>(I)V",            // a type parameter with no bound marker
            "(TT)V",              // a type variable with no `;`
        ] {
            assert!(render_method(bad).is_none(), "'{bad}' must not render as a method, but it did");
        }
    }

    /// A signature with no type arguments must render exactly what the plain descriptor renderer would, so
    /// that preferring the generic form cannot change an answer that was already right.
    #[test]
    fn an_ungeneric_signature_renders_what_the_plain_one_does() {
        for (sig, want) in [
            ("Ljava/lang/String;", "java.lang.String"),
            ("[Ljava/lang/String;", "java.lang.String[]"),
            ("I", "int"),
            ("[[D", "double[][]"),
            ("Lcom/x/Order$Line;", "com.x.Order$Line"),
        ] {
            assert_eq!(render_type(sig).as_deref(), Some(want), "{sig}");
        }
    }
}
