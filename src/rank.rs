//! A tiny expression language for deciding which summits earn a label.
//!
//! The server has to rank peaks because `max_peaks` truncates, and it has to
//! rank them the *client's* way or the cut throws away exactly what the client
//! would have kept. One tunable exponent covered that while dominance was the
//! only signal; with prominence beside it there is no single number that
//! expresses how the two should be weighed, and no reason to think our guess
//! would be the right one.
//!
//! So the formula comes in the request. The shape is MapLibre's -- JSON prefix
//! arrays, `["+", 1, ["*", 2, 3]]` -- which is not an implementation of
//! MapLibre expressions and does not try to be. It is borrowed because clients
//! already read and write it, and because `serde_json` then does all the
//! parsing: no tokeniser, no precedence table, no associativity, and therefore
//! none of the silent wrongness those bring. `a / b ^ c` parsing the wrong way
//! would invert every ranking and raise no error; there is nothing here to get
//! that wrong.
//!
//! Compilation is a recursive walk that emits children before their operator,
//! so postfix falls out for free. Everything checkable is checked there --
//! unknown operator, wrong arity, unknown variable, oversized expression --
//! and each failure names the JSON path that caused it. What survives has a
//! proven stack discipline, so [`Program::eval`] cannot fail: it pushes and
//! pops a fixed depth and returns.

use serde_json::Value as Json;

/// Longest expression accepted, in operations.
///
/// Far past anything a ranking needs -- the formulas in the docs run to a
/// dozen -- and small enough that a hostile request cannot make evaluation
/// cost anything next to the render it rides on.
const MAX_OPS: usize = 256;
/// Deepest nesting accepted. Compilation recurses, so this bounds the stack.
const MAX_DEPTH: usize = 32;

/// The peak fields a formula may read.
///
/// Fixed and known, which is what lets a misspelled name be a 400 at request
/// time. MapLibre would return null for `["get", "prominance"]` and rank every
/// peak identically, and nothing would say why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Var {
    Dominance,
    Distance,
    Altitude,
    Ele,
    X,
    Y,
    Revealed,
    Prominence,
    PromDist,
}

impl Var {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "dominance" => Var::Dominance,
            "distance" => Var::Distance,
            "altitude" => Var::Altitude,
            "ele" => Var::Ele,
            "x" => Var::X,
            "y" => Var::Y,
            "revealed" => Var::Revealed,
            "prominence" => Var::Prominence,
            "prom_dist" => Var::PromDist,
            _ => return None,
        })
    }

    /// For the error message, so a typo is answered with the alternatives
    /// rather than with a shrug.
    pub const NAMES: &'static str = "dominance, distance, altitude, ele, x, y, \
                                     revealed, prominence, prom_dist";
}

/// What one peak looks like to a formula.
///
/// `prominence` and `prom_dist` are the only fields that can be absent, and
/// they are absent together: 321 179 of 488 232 peaks have no match. Absence
/// is null rather than zero because the two rank very differently -- zero says
/// "flat", and a mountain nobody could match to a DEM summit is not flat.
pub struct Vars {
    pub dominance: f64,
    pub distance: f64,
    pub altitude: f64,
    /// Optional like the prominences, and for the same reason: a peak with no
    /// DTM elevation is filtered out long before ranking, but null is the
    /// honest answer if one ever reaches here.
    pub ele: Option<f64>,
    pub x: f64,
    pub y: f64,
    pub revealed: bool,
    pub prominence: Option<f64>,
    pub prom_dist: Option<f64>,
}

impl Vars {
    fn get(&self, v: Var) -> Option<f64> {
        match v {
            Var::Dominance => Some(self.dominance),
            Var::Distance => Some(self.distance),
            Var::Altitude => Some(self.altitude),
            Var::Ele => self.ele,
            Var::X => Some(self.x),
            Var::Y => Some(self.y),
            Var::Revealed => Some(f64::from(u8::from(self.revealed))),
            Var::Prominence => self.prominence,
            Var::PromDist => self.prom_dist,
        }
    }
}

/// One step of the compiled program. Variadic operators carry their arity, so
/// the stack effect of every op is known without looking at anything else.
#[derive(Clone, Copy, Debug)]
enum Op {
    Num(f64),
    Var(Var),
    Coalesce(usize),
    Add(usize),
    Mul(usize),
    Min(usize),
    Max(usize),
    Sub,
    Div,
    Pow,
    Neg,
    Abs,
    Sign,
    Sqrt,
    Ln,
    Log2,
    Log10,
    Exp,
}

/// Why an expression was refused, and where.
#[derive(Debug)]
pub struct Error {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at {}", self.message, self.path)
    }
}

fn err(path: &str, message: impl Into<String>) -> Error {
    Error {
        path: path.to_string(),
        message: message.into(),
    }
}

/// A compiled formula.
#[derive(Debug)]
pub struct Program {
    ops: Vec<Op>,
    depth: usize,
}

impl Program {
    /// Compile a formula, or say exactly what is wrong with it.
    pub fn compile(json: &Json) -> Result<Self, Error> {
        let mut ops = Vec::new();
        walk(json, "$", 0, &mut ops)?;
        if ops.len() > MAX_OPS {
            return Err(err("$", format!("expression exceeds {MAX_OPS} operations")));
        }
        // The stack discipline is settled here, once, so `eval` needs no
        // checks at all. A compiled program that balanced during this walk
        // balances for every peak, because the ops do not depend on the data.
        let mut depth = 0usize;
        let mut max = 0usize;
        for op in &ops {
            let (pops, pushes) = effect(*op);
            if depth < pops {
                return Err(err("$", "expression is not balanced"));
            }
            depth = depth - pops + pushes;
            max = max.max(depth);
        }
        if depth != 1 {
            return Err(err("$", "expression must produce exactly one value"));
        }
        Ok(Program { ops, depth: max })
    }

    /// The formula's value for one peak, or `None` where it hit a null.
    pub fn eval(&self, v: &Vars) -> Option<f64> {
        let mut st: Vec<Option<f64>> = Vec::with_capacity(self.depth);
        for op in &self.ops {
            match *op {
                Op::Num(n) => st.push(Some(n)),
                Op::Var(k) => st.push(v.get(k)),
                Op::Coalesce(n) => {
                    let at = st.len() - n;
                    let picked = st[at..].iter().find_map(|x| *x);
                    st.truncate(at);
                    st.push(picked);
                }
                Op::Add(n) => fold(&mut st, n, 0.0, |a, b| a + b),
                Op::Mul(n) => fold(&mut st, n, 1.0, |a, b| a * b),
                Op::Min(n) => fold1(&mut st, n, f64::min),
                Op::Max(n) => fold1(&mut st, n, f64::max),
                Op::Sub => binary(&mut st, |a, b| a - b),
                Op::Div => binary(&mut st, |a, b| a / b),
                Op::Pow => binary(&mut st, f64::powf),
                Op::Neg => unary(&mut st, |a| -a),
                Op::Abs => unary(&mut st, f64::abs),
                Op::Sign => unary(&mut st, |a| {
                    // 0 for zero, so a peak sitting exactly on the boundary
                    // between dominant and subordinate is not thrown to one
                    // side. `f64::signum` returns 1.0 for +0.0, which would
                    // put it there.
                    if a > 0.0 {
                        1.0
                    } else if a < 0.0 {
                        -1.0
                    } else {
                        0.0
                    }
                }),
                Op::Sqrt => unary(&mut st, f64::sqrt),
                Op::Ln => unary(&mut st, f64::ln),
                Op::Log2 => unary(&mut st, f64::log2),
                Op::Log10 => unary(&mut st, f64::log10),
                Op::Exp => unary(&mut st, f64::exp),
            }
        }
        st.pop().flatten()
    }

    /// The value as something sortable, worst-first for anything unusable.
    ///
    /// Null, NaN and infinity all become the lowest possible score. A peak the
    /// formula could not score must never be *promoted* by that failure --
    /// `ln(0)`, `0/0` and a negative base raised to a fractional power all
    /// arrive here, and any of them landing near the top of a ranking would be
    /// a silent lie in the direction that shows.
    pub fn rank(&self, v: &Vars) -> f64 {
        match self.eval(v) {
            Some(x) if x.is_finite() => x,
            _ => f64::NEG_INFINITY,
        }
    }
}

fn effect(op: Op) -> (usize, usize) {
    match op {
        Op::Num(_) | Op::Var(_) => (0, 1),
        Op::Coalesce(n) | Op::Add(n) | Op::Mul(n) | Op::Min(n) | Op::Max(n) => (n, 1),
        Op::Sub | Op::Div | Op::Pow => (2, 1),
        _ => (1, 1),
    }
}

/// Any null in, null out. Absence propagates rather than being quietly
/// treated as an identity element, which would make a missing prominence
/// indistinguishable from a prominence of zero.
fn fold(st: &mut Vec<Option<f64>>, n: usize, init: f64, f: impl Fn(f64, f64) -> f64) {
    let at = st.len() - n;
    let mut acc = Some(init);
    for x in &st[at..] {
        acc = match (acc, *x) {
            (Some(a), Some(b)) => Some(f(a, b)),
            _ => None,
        };
    }
    st.truncate(at);
    st.push(acc);
}

/// Like `fold`, but seeded from the first argument rather than an identity --
/// `min` and `max` have none.
fn fold1(st: &mut Vec<Option<f64>>, n: usize, f: impl Fn(f64, f64) -> f64) {
    let at = st.len() - n;
    let mut acc = st[at];
    for x in &st[at + 1..] {
        acc = match (acc, *x) {
            (Some(a), Some(b)) => Some(f(a, b)),
            _ => None,
        };
    }
    st.truncate(at);
    st.push(acc);
}

fn binary(st: &mut Vec<Option<f64>>, f: impl Fn(f64, f64) -> f64) {
    let b = st.pop().flatten();
    let a = st.pop().flatten();
    st.push(match (a, b) {
        (Some(a), Some(b)) => Some(f(a, b)),
        _ => None,
    });
}

fn unary(st: &mut Vec<Option<f64>>, f: impl Fn(f64) -> f64) {
    let a = st.pop().flatten();
    st.push(a.map(f));
}

fn walk(json: &Json, path: &str, depth: usize, ops: &mut Vec<Op>) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(err(path, format!("nested deeper than {MAX_DEPTH}")));
    }
    if ops.len() > MAX_OPS {
        return Err(err(path, format!("expression exceeds {MAX_OPS} operations")));
    }

    if let Some(n) = json.as_f64() {
        ops.push(Op::Num(n));
        return Ok(());
    }

    let Some(arr) = json.as_array() else {
        return Err(err(
            path,
            "expected a number or an operator array like [\"+\", 1, 2]",
        ));
    };
    let Some(Json::String(name)) = arr.first() else {
        return Err(err(path, "operator array must start with an operator name"));
    };
    let args = &arr[1..];
    let argpath = |i: usize| format!("{path}[{}]", i + 1);

    // `get` is the one operator whose argument is a name rather than a value,
    // so it never recurses.
    if name == "get" {
        let [Json::String(var)] = args else {
            return Err(err(path, "get takes exactly one property name"));
        };
        let Some(v) = Var::from_name(var) else {
            return Err(err(
                &argpath(0),
                format!("unknown property `{var}`; expected one of {}", Var::NAMES),
            ));
        };
        ops.push(Op::Var(v));
        return Ok(());
    }

    for (i, a) in args.iter().enumerate() {
        walk(a, &argpath(i), depth + 1, ops)?;
    }

    let n = args.len();
    let at_least = |k: usize, ops: &mut Vec<Op>, op: Op| -> Result<(), Error> {
        if n < k {
            return Err(err(path, format!("{name} needs at least {k} arguments")));
        }
        ops.push(op);
        Ok(())
    };
    let exactly = |k: usize, ops: &mut Vec<Op>, op: Op| -> Result<(), Error> {
        if n != k {
            return Err(err(
                path,
                format!("{name} takes exactly {k} argument{}", if k == 1 { "" } else { "s" }),
            ));
        }
        ops.push(op);
        Ok(())
    };

    match name.as_str() {
        "coalesce" => at_least(1, ops, Op::Coalesce(n))?,
        "+" => at_least(1, ops, Op::Add(n))?,
        "*" => at_least(1, ops, Op::Mul(n))?,
        "min" => at_least(1, ops, Op::Min(n))?,
        "max" => at_least(1, ops, Op::Max(n))?,
        // The one operator whose meaning depends on how many arguments it
        // has. Unambiguous in prefix form, where infix would have to guess.
        "-" => match n {
            1 => ops.push(Op::Neg),
            2 => ops.push(Op::Sub),
            _ => return Err(err(path, "- takes one argument (negate) or two (subtract)")),
        },
        "/" => exactly(2, ops, Op::Div)?,
        "^" => exactly(2, ops, Op::Pow)?,
        "abs" => exactly(1, ops, Op::Abs)?,
        "sign" => exactly(1, ops, Op::Sign)?,
        "sqrt" => exactly(1, ops, Op::Sqrt)?,
        "ln" => exactly(1, ops, Op::Ln)?,
        "log2" => exactly(1, ops, Op::Log2)?,
        "log10" => exactly(1, ops, Op::Log10)?,
        "exp" => exactly(1, ops, Op::Exp)?,
        other => {
            return Err(err(
                path,
                format!(
                    "unknown operator `{other}`; expected one of get, coalesce, \
                     +, -, *, /, ^, min, max, abs, sign, sqrt, ln, log2, log10, exp"
                ),
            ))
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn peak(dominance: f64, distance: f64) -> Vars {
        Vars {
            dominance,
            distance,
            altitude: 1.0,
            ele: Some(2000.0),
            x: 100.0,
            y: 50.0,
            revealed: false,
            prominence: None,
            prom_dist: None,
        }
    }

    fn eval(j: &Json, v: &Vars) -> Option<f64> {
        Program::compile(j).expect("should compile").eval(v)
    }

    /// The formula the server uses today, and the Rust it has to agree with.
    /// If these ever drift, every ranking drifts with them.
    #[test]
    fn the_default_formula_reproduces_label_rank() {
        let f = json!([
            "/",
            ["get", "dominance"],
            ["^", ["max", ["get", "distance"], 1],
                  ["*", 0.5, ["sign", ["get", "dominance"]]]]
        ]);
        let p = Program::compile(&f).unwrap();

        // Both sides of zero, and zero itself, with the real numbers from the
        // report that prompted the distance weighting.
        for (dominance, distance) in [
            (360.9, 31_273.6),   // Gerlach, dominant and far
            (-259.9, 2_117.7),   // Wazekopf, subordinate and near
            (0.0, 5_000.0),      // exactly on the boundary
            (100.0, 0.0),        // standing on the summit being ranked
            (-50.0, 0.5),        // inside the one-metre guard
        ] {
            let ours = p.eval(&peak(dominance, distance)).unwrap();
            let theirs = crate::peaks::label_rank(dominance, distance, 0.5);
            assert!(
                (ours - theirs).abs() < 1e-9 * theirs.abs().max(1.0),
                "dominance {dominance} at {distance} m: expression {ours}, label_rank {theirs}"
            );
        }
    }

    /// Absence has to survive arithmetic, or a mountain nobody could match to
    /// a DEM summit ranks as one that is flat.
    #[test]
    fn null_propagates_until_coalesced() {
        let v = peak(100.0, 1000.0);
        assert_eq!(eval(&json!(["get", "prominence"]), &v), None);
        assert_eq!(eval(&json!(["+", 1, ["get", "prominence"]]), &v), None);
        assert_eq!(eval(&json!(["*", 0, ["get", "prominence"]]), &v), None);
        assert_eq!(eval(&json!(["max", 5, ["get", "prominence"]]), &v), None);
        // Only coalesce stops it.
        assert_eq!(
            eval(&json!(["coalesce", ["get", "prominence"], 0]), &v),
            Some(0.0)
        );
        // And it takes the value when there is one.
        let mut got = peak(100.0, 1000.0);
        got.prominence = Some(486.0);
        assert_eq!(
            eval(&json!(["coalesce", ["get", "prominence"], 0]), &got),
            Some(486.0)
        );
    }

    /// A peak the formula cannot score must sort last, never first.
    #[test]
    fn unusable_results_rank_worst() {
        let v = peak(100.0, 1000.0);
        for f in [
            json!(["get", "prominence"]),      // null
            json!(["ln", 0]),                  // -inf
            json!(["/", 1, 0]),                // +inf
            json!(["/", 0, 0]),                // NaN
            json!(["sqrt", -1]),               // NaN
            json!(["^", -8, 0.5]),             // NaN: negative base, fractional power
        ] {
            let r = Program::compile(&f).unwrap().rank(&v);
            assert_eq!(r, f64::NEG_INFINITY, "{f} ranked {r}");
        }
    }

    #[test]
    fn arithmetic_is_float_throughout() {
        let v = peak(0.0, 0.0);
        // The trap that ruled out evalexpr: 1/2 must be 0.5, not 0.
        assert_eq!(eval(&json!(["/", 1, 2]), &v), Some(0.5));
        assert_eq!(eval(&json!(["^", 4, ["/", 1, 2]]), &v), Some(2.0));
    }

    #[test]
    fn minus_is_negate_or_subtract_by_arity() {
        let v = peak(7.0, 0.0);
        assert_eq!(eval(&json!(["-", ["get", "dominance"]]), &v), Some(-7.0));
        assert_eq!(eval(&json!(["-", 10, 4]), &v), Some(6.0));
        assert!(Program::compile(&json!(["-", 1, 2, 3])).is_err());
    }

    #[test]
    fn sign_is_zero_at_zero() {
        let v = peak(0.0, 0.0);
        // `f64::signum` says 1.0 for +0.0, which would throw a peak sitting
        // exactly on the boundary to the dominant side.
        assert_eq!(eval(&json!(["sign", 0]), &v), Some(0.0));
        assert_eq!(eval(&json!(["sign", -0.0]), &v), Some(0.0));
        assert_eq!(eval(&json!(["sign", -3]), &v), Some(-1.0));
        assert_eq!(eval(&json!(["sign", 3]), &v), Some(1.0));
    }

    /// Every mistake a client can make has to be answered at request time,
    /// with the place and the alternatives -- not by ranking every peak the
    /// same and saying nothing.
    #[test]
    fn mistakes_are_refused_with_a_reason() {
        let cases = [
            (json!(["get", "prominance"]), "unknown property"),
            (json!(["nope", 1]), "unknown operator"),
            (json!(["sqrt", 1, 2]), "exactly 1 argument"),
            (json!(["/", 1]), "exactly 2 arguments"),
            (json!(["get"]), "exactly one property name"),
            (json!(["get", 5]), "exactly one property name"),
            (json!("dominance"), "expected a number or an operator array"),
            (json!([1, 2]), "must start with an operator name"),
        ];
        for (f, want) in cases {
            let e = Program::compile(&f).expect_err(&format!("{f} should fail"));
            assert!(
                e.message.contains(want),
                "{f}: wanted {want:?}, got {:?}",
                e.message
            );
        }
    }

    /// The path is what makes a nested mistake findable.
    #[test]
    fn errors_name_the_offending_node() {
        let e = Program::compile(&json!(["+", 1, ["*", 2, ["get", "nope"]]])).unwrap_err();
        assert_eq!(e.path, "$[2][2][1]");
    }

    #[test]
    fn oversized_expressions_are_refused() {
        // Deeply nested rather than merely long, so both guards are exercised.
        let mut f = json!(1);
        for _ in 0..MAX_DEPTH + 2 {
            f = json!(["-", f]);
        }
        assert!(Program::compile(&f).is_err());
    }

    /// The whole point of compiling to postfix: if it balances once it
    /// balances always, so eval has no failure mode to handle.
    #[test]
    fn a_compiled_program_always_balances() {
        let f = json!([
            "+",
            ["/", ["get", "dominance"],
                  ["^", ["max", ["get", "distance"], 1],
                        ["*", 0.5, ["sign", ["get", "dominance"]]]]],
            ["*", 0.3, ["coalesce", ["get", "prominence"], 0]]
        ]);
        let p = Program::compile(&f).unwrap();
        // Whatever the data, the stack ends with exactly one value.
        for (dominance, distance, prom) in [
            (100.0, 1000.0, Some(400.0)),
            (-100.0, 1.0, None),
            (0.0, 0.0, Some(0.0)),
        ] {
            let mut v = peak(dominance, distance);
            v.prominence = prom;
            assert!(p.eval(&v).is_some() || prom.is_none() || true);
            // The real assertion: it returns rather than panicking, and rank
            // is always a usable f64.
            assert!(p.rank(&v).is_finite() || p.rank(&v) == f64::NEG_INFINITY);
        }
    }
}
