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
            // Spelled exactly as the payload spells it. Two names for one
            // field is how a client ends up learning the schema by dumping
            // keys from a response.
            "prom_dist_m" => Var::PromDist,
            _ => return None,
        })
    }

    /// For the error message, so a typo is answered with the alternatives
    /// rather than with a shrug.
    pub const NAMES: &'static str = "dominance, distance, altitude, ele, x, y, \
                                     revealed, prominence, prom_dist_m";
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
    /// Comparisons yield 1 or 0 rather than a boolean, so the value type stays
    /// `number | null` and nothing else has to learn about truth.
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Pops a condition and jumps if it is not true. Null and NaN are not
    /// true: an unknown is not a yes.
    JumpIfFalsy(usize),
    Jump(usize),
}

/// A condition is true only if it is a number that is neither zero nor NaN.
///
/// Null falls through to the next branch rather than making the whole `case`
/// null, which is the point of having `case` at all: `["*", c, a]` cannot
/// express a choice when `a` may be absent, because null times anything is
/// null even where the branch was never wanted.
fn truthy(v: Option<f64>) -> bool {
    matches!(v, Some(x) if x != 0.0 && !x.is_nan())
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
        // The stack discipline is settled here, once, so `eval` needs no
        // checks at all. It cannot be verified by scanning the ops linearly
        // any more -- `case` compiles to jumps, and a linear scan would count
        // every branch as if all of them ran. It is instead an invariant of
        // the walk: every expression node leaves exactly one value behind,
        // whichever path is taken through it.
        let mut c = Compiler {
            ops: Vec::new(),
            depth: 0,
            max: 0,
        };
        c.walk(json, "$", 0)?;
        if c.depth != 1 {
            return Err(err("$", "expression must produce exactly one value"));
        }
        Ok(Program {
            ops: c.ops,
            depth: c.max,
        })
    }

    /// The formula's value for one peak, or `None` where it hit a null.
    pub fn eval(&self, v: &Vars) -> Option<f64> {
        let mut st: Vec<Option<f64>> = Vec::with_capacity(self.depth);
        let mut pc = 0usize;
        while pc < self.ops.len() {
            match self.ops[pc] {
                Op::Jump(t) => {
                    pc = t;
                    continue;
                }
                Op::JumpIfFalsy(t) => {
                    let c = st.pop().flatten();
                    if !truthy(c) {
                        pc = t;
                        continue;
                    }
                }
                Op::Eq => compare(&mut st, |a, b| a == b),
                Op::Ne => compare(&mut st, |a, b| a != b),
                Op::Lt => compare(&mut st, |a, b| a < b),
                Op::Le => compare(&mut st, |a, b| a <= b),
                Op::Gt => compare(&mut st, |a, b| a > b),
                Op::Ge => compare(&mut st, |a, b| a >= b),
                op => self.step(op, &mut st, v),
            }
            pc += 1;
        }
        st.pop().flatten()
    }

    fn step(&self, op: Op, st: &mut Vec<Option<f64>>, v: &Vars) {
        {
            match op {
                Op::Num(n) => st.push(Some(n)),
                Op::Var(k) => st.push(v.get(k)),
                Op::Coalesce(n) => {
                    let at = st.len() - n;
                    let picked = st[at..].iter().find_map(|x| *x);
                    st.truncate(at);
                    st.push(picked);
                }
                Op::Add(n) => fold(st,n, 0.0, |a, b| a + b),
                Op::Mul(n) => fold(st,n, 1.0, |a, b| a * b),
                // Not `f64::min`/`f64::max`: those are IEEE minNum/maxNum and
                // discard a NaN operand, so `["max", ["/",0,0], 5]` would
                // score 5 and rank normally, and a peak the formula could not
                // score must never be *promoted* by that failure.
                //
                // Every arithmetic operator propagates badness this way. The
                // comparisons deliberately do not -- `["<", NaN, 5]` is 0,
                // not null -- so a `case` on a NaN takes its else branch
                // rather than yielding null. That is the one family this rule
                // does not cover, and it is the family filters are built from.
                Op::Min(n) => fold1(st, n, |a, b| if a.is_nan() || b.is_nan() { f64::NAN } else { a.min(b) }),
                Op::Max(n) => fold1(st, n, |a, b| if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(b) }),
                Op::Sub => binary(st,|a, b| a - b),
                Op::Div => binary(st,|a, b| a / b),
                Op::Pow => binary(st,f64::powf),
                Op::Neg => unary(st,|a| -a),
                Op::Abs => unary(st,f64::abs),
                Op::Sign => unary(st,|a| {
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
                Op::Sqrt => unary(st,f64::sqrt),
                Op::Ln => unary(st,f64::ln),
                Op::Log2 => unary(st,f64::log2),
                Op::Log10 => unary(st,f64::log10),
                Op::Exp => unary(st,f64::exp),
                // Handled by the caller, which owns the program counter.
                Op::Jump(_)
                | Op::JumpIfFalsy(_)
                | Op::Eq
                | Op::Ne
                | Op::Lt
                | Op::Le
                | Op::Gt
                | Op::Ge => unreachable!("control flow is dispatched in eval"),
            }
        }
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

    /// Whether a peak survives this expression used as a filter.
    ///
    /// Null is not a yes, the same rule `case` uses for its conditions. So
    /// `[">", ["get","prominence"], 100]` drops every peak with no prominence
    /// -- two thirds of them -- rather than keeping them on the grounds that
    /// nothing disproved the test. `coalesce` is how a caller says what an
    /// absence should count as.
    pub fn keeps(&self, v: &Vars) -> bool {
        truthy(self.eval(v))
    }
}

/// Emits ops while tracking what they do to the stack.
struct Compiler {
    ops: Vec<Op>,
    /// Values on the stack at this point in the program.
    depth: usize,
    /// The most there will ever be, which is all `eval` needs to allocate.
    max: usize,
}

impl Compiler {
    fn push(&mut self, op: Op, pops: usize) {
        self.ops.push(op);
        // The linear balance scan went when `case` brought jumps, and this is
        // what replaced its underflow check. Unreachable today -- both call
        // sites pass 0 -- but release builds have overflow checks off, so a
        // future caller getting it wrong would wrap to ~1.8e19, carry that
        // into `max`, and reach `Vec::with_capacity`: an allocation abort
        // instead of a compile error.
        debug_assert!(self.depth >= pops, "stack underflow while compiling");
        self.depth = self.depth.saturating_sub(pops) + 1;
        self.max = self.max.max(self.depth);
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

/// Comparisons return 1 or 0, and null in gives null out -- an unknown
/// compares to nothing, and saying "false" would let a formula act on it.
fn compare(st: &mut Vec<Option<f64>>, f: impl Fn(f64, f64) -> bool) {
    let b = st.pop().flatten();
    let a = st.pop().flatten();
    st.push(match (a, b) {
        (Some(a), Some(b)) => Some(f64::from(u8::from(f(a, b)))),
        _ => None,
    });
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

impl Compiler {
fn walk(&mut self, json: &Json, path: &str, depth: usize) -> Result<(), Error> {
    let ops = &mut self.ops;
    if depth > MAX_DEPTH {
        return Err(err(path, format!("nested deeper than {MAX_DEPTH}")));
    }
    if ops.len() > MAX_OPS {
        return Err(err(path, format!("expression exceeds {MAX_OPS} operations")));
    }

    if let Some(n) = json.as_f64() {
        self.push(Op::Num(n), 0);
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
        self.push(Op::Var(v), 0);
        return Ok(());
    }

    // `case` compiles to jumps rather than to a value-consuming operator,
    // because a choice has to *not evaluate* the branch it did not take.
    // Multiplying by a 0/1 indicator cannot express that: null times zero is
    // null, so a branch reading an absent prominence poisons the answer even
    // where it was never wanted.
    if name == "case" {
        if args.len() < 3 || args.len() % 2 == 0 {
            return Err(err(
                path,
                "case takes a condition, a value, optionally more pairs, \
                 and a final fallback -- an odd number, at least three",
            ));
        }
        let base = self.depth;
        let mut max = base;
        let mut ends: Vec<usize> = Vec::new();
        let mut i = 0;
        while i + 1 < args.len() {
            self.depth = base;
            self.walk(&args[i], &argpath(i), depth + 1)?;
            let jf = self.ops.len();
            self.ops.push(Op::JumpIfFalsy(usize::MAX));
            self.depth -= 1; // the condition is consumed by the jump
            self.walk(&args[i + 1], &argpath(i + 1), depth + 1)?;
            max = max.max(self.depth);
            ends.push(self.ops.len());
            self.ops.push(Op::Jump(usize::MAX));
            let here = self.ops.len();
            self.ops[jf] = Op::JumpIfFalsy(here);
            i += 2;
        }
        // The fallback. Every path arrives here having pushed nothing, and
        // leaves having pushed exactly one value -- which is what makes the
        // whole construct behave like any other expression node.
        self.depth = base;
        self.walk(&args[args.len() - 1], &argpath(args.len() - 1), depth + 1)?;
        max = max.max(self.depth);
        let end = self.ops.len();
        for e in ends {
            self.ops[e] = Op::Jump(end);
        }
        self.depth = base + 1;
        self.max = self.max.max(max);
        return Ok(());
    }

    for (i, a) in args.iter().enumerate() {
        self.walk(a, &argpath(i), depth + 1)?;
    }

    let n = args.len();
    let ops = &mut self.ops;
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
        "==" => exactly(2, ops, Op::Eq)?,
        "!=" => exactly(2, ops, Op::Ne)?,
        "<" => exactly(2, ops, Op::Lt)?,
        "<=" => exactly(2, ops, Op::Le)?,
        ">" => exactly(2, ops, Op::Gt)?,
        ">=" => exactly(2, ops, Op::Ge)?,
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
                    "unknown operator `{other}`; expected one of get, coalesce, case, \
                     +, -, *, /, ^, ==, !=, <, <=, >, >=, min, max, abs, sign, sqrt, \
                     ln, log2, log10, exp"
                ),
            ))
        }
    }
    // Every operator above pops its arguments and pushes one result. `case`
    // returned earlier, having done its own accounting.
    //
    // This is the subtraction that could actually underflow -- every non-case
    // operator goes through it, where `Compiler::push` is only ever called
    // with zero. It holds because each of the `n` arguments was walked and
    // each walk leaves exactly one value, and because no zero-arity operator
    // reaches here: `get` returns early, and every other arity check rejects
    // n = 0 first. That is the invariant worth asserting, and a future
    // operator taking a name argument the way `get` does is what would break
    // it.
    debug_assert!(self.depth >= n, "{name} popped more than it pushed");
    self.depth = self.depth.saturating_sub(n) + 1;
    self.max = self.max.max(self.depth);
    Ok(())
}
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

    /// The sign folded into the exponent is the whole trick, so it is checked
    /// against arithmetic written out longhand rather than against another
    /// copy of itself. There used to be a second implementation in Rust and a
    /// test that the two agreed; deleting it is what made the documented
    /// default formula the only one there is.
    #[test]
    fn the_default_formula_weights_distance_on_both_sides_of_zero() {
        // The *shipped* default, not a copy of it typed here. A local `json!`
        // would test itself: changing 0.5 to 0.25 in `peaks::default_rank`
        // would leave this green while silently falsifying the documentation,
        // because every other assertion about the default is a monotonicity
        // or sign property that any positive exponent satisfies.
        let p = crate::peaks::default_rank();

        // Real numbers from the report that prompted the distance weighting.
        for (dominance, distance) in [
            (360.9f64, 31_273.6f64), // Gerlach, dominant and far
            (-259.9, 2_117.7),       // Wazekopf, subordinate and near
            (0.0, 5_000.0),          // exactly on the boundary
            (100.0, 0.0),            // standing on the summit being ranked
            (-50.0, 0.5),            // inside the one-metre guard
        ] {
            let scale = distance.max(1.0).sqrt();
            let want = if dominance > 0.0 {
                dominance / scale
            } else if dominance < 0.0 {
                dominance * scale
            } else {
                0.0
            };
            let got = p.eval(&peak(dominance, distance)).unwrap();
            assert!(
                (got - want).abs() < 1e-9 * want.abs().max(1.0),
                "dominance {dominance} at {distance} m: got {got}, want {want}"
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

    /// The reason `case` exists at all: a choice must not evaluate the branch
    /// it did not take. `["*", cond, x]` cannot express one, because null
    /// times zero is null -- so a branch reading an absent prominence poisons
    /// the answer even when the condition said not to look at it.
    #[test]
    fn case_does_not_evaluate_the_branch_it_skips() {
        let v = peak(100.0, 1000.0); // prominence is None
        // Arithmetic selection: the untaken branch still poisons it.
        let poisoned = eval(
            &json!(["+", ["*", 0, ["get", "prominence"]], ["*", 1, 42]]),
            &v,
        );
        assert_eq!(poisoned, None, "arithmetic selection should still poison");

        // case does not.
        let f = json!(["case", ["==", 1, 0], ["get", "prominence"], 42]);
        assert_eq!(eval(&f, &v), Some(42.0));

        // And it takes the branch when the condition holds.
        let f = json!(["case", ["==", 1, 1], 7, ["get", "prominence"]]);
        assert_eq!(eval(&f, &v), Some(7.0));
    }

    #[test]
    fn comparisons_yield_one_or_zero_and_pass_null_through() {
        let v = peak(100.0, 1000.0);
        assert_eq!(eval(&json!(["<", 1, 2]), &v), Some(1.0));
        assert_eq!(eval(&json!([">", 1, 2]), &v), Some(0.0));
        assert_eq!(eval(&json!([">=", 2, 2]), &v), Some(1.0));
        assert_eq!(eval(&json!(["==", ["get", "dominance"], 100]), &v), Some(1.0));
        // An unknown compares to nothing.
        assert_eq!(eval(&json!(["<", ["get", "prominence"], 100]), &v), None);
    }

    /// A null or NaN condition is not true, so it falls through to the next
    /// branch. Saying "false" would be the same answer but the wrong reason;
    /// what matters is that it never *takes* a branch on an unknown.
    #[test]
    fn an_unknown_condition_falls_through() {
        let v = peak(100.0, 1000.0);
        let f = json!(["case", ["<", ["get", "prominence"], 100], 1, 2]);
        assert_eq!(eval(&f, &v), Some(2.0));
        let f = json!(["case", ["/", 0, 0], 1, 2]);
        assert_eq!(eval(&f, &v), Some(2.0));
    }

    #[test]
    fn case_chains_and_checks_its_shape() {
        let v = peak(100.0, 1000.0);
        let f = json!([
            "case",
            [">", ["get", "dominance"], 1000], 1,
            [">", ["get", "dominance"], 50], 2,
            3
        ]);
        assert_eq!(eval(&f, &v), Some(2.0));

        // Every branch is compiled, including ones this peak will never
        // reach, so a malformed branch is a 400 at request time rather than a
        // surprise for whichever viewpoint first takes that path.
        let unreachable_but_broken = json!([
            "case",
            [">", ["get", "dominance"], 1000], "big",
            2
        ]);
        assert!(Program::compile(&unreachable_but_broken).is_err());

        // An even argument count means a missing fallback.
        assert!(Program::compile(&json!(["case", 1, 2])).is_err());
        assert!(Program::compile(&json!(["case", 1, 2, 3, 4])).is_err());
        assert!(Program::compile(&json!(["case", 1, 2, 3])).is_ok());
    }

    /// Jumps make the linear stack scan useless, so depth is now an invariant
    /// of the walk. This is the assertion that it still holds.
    #[test]
    fn nested_cases_stay_balanced() {
        let v = peak(-40.0, 8000.0);
        let f = json!([
            "+",
            ["case", [">", ["get", "dominance"], 0],
                     ["case", [">", ["get", "distance"], 5000], 1, 2],
                     ["case", [">", ["get", "distance"], 5000], 3, 4]],
            10
        ]);
        assert_eq!(eval(&f, &v), Some(13.0));
    }

    /// The path is what makes a nested mistake findable.
    #[test]
    fn errors_name_the_offending_node() {
        let e = Program::compile(&json!(["+", 1, ["*", 2, ["get", "nope"]]])).unwrap_err();
        assert_eq!(e.path, "$[2][2][1]");
    }

    #[test]
    fn oversized_expressions_are_refused() {
        // Deep: 34 wrappers against a limit of 32.
        let mut f = json!(1);
        for _ in 0..MAX_DEPTH + 2 {
            f = json!(["-", f]);
        }
        assert!(Program::compile(&f).is_err(), "depth limit");

        // Long: this is what the old comment claimed to cover and did not --
        // 34 nested ops are 34 ops, nowhere near 256, so only the depth guard
        // ever fired and MAX_OPS had no test at all.
        let wide: Vec<Json> = std::iter::once(json!("+"))
            .chain(std::iter::repeat_n(json!(1), MAX_OPS * 4))
            .collect();
        assert!(Program::compile(&Json::Array(wide)).is_err(), "op limit");

        // And a shape that grows through `case`, whose jumps are emitted
        // outside the counting path.
        let mut chain: Vec<Json> = vec![json!("case")];
        for _ in 0..MAX_OPS {
            chain.push(json!([">", ["get", "dominance"], 1]));
            chain.push(json!(1));
        }
        chain.push(json!(0));
        assert!(Program::compile(&Json::Array(chain)).is_err(), "case chain");
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
        // Whatever the data, the answer is the arithmetic -- checked against
        // it, not against "did not panic". The previous version of this test
        // asserted `x || y || true`, which is a tautology, and would have
        // passed had `eval` returned anything at all.
        for (dominance, distance, prom) in [
            (100.0, 1000.0, Some(400.0)),
            (-100.0, 1.0, None),
            (0.0, 0.0, Some(0.0)),
            (-259.9, 2117.7, Some(486.0)),
        ] {
            let mut v = peak(dominance, distance);
            v.prominence = prom;
            let scale = distance.max(1.0f64).sqrt();
            let base = if dominance > 0.0 {
                dominance / scale
            } else if dominance < 0.0 {
                dominance * scale
            } else {
                0.0
            };
            let want = base + 0.3 * prom.unwrap_or(0.0);
            let got = p.eval(&v).expect("coalesce makes this total");
            assert!(
                (got - want).abs() < 1e-9 * want.abs().max(1.0),
                "{dominance} at {distance} with {prom:?}: got {got}, want {want}"
            );
        }
    }
}
