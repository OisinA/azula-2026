use std::rc::Rc;

use azula_type::prelude::AzulaType;

#[derive(Debug, PartialEq, Clone)]
pub enum Statement<'a> {
    Root(Body<'a>),
    Block(Body<'a>),
    Function {
        name: &'a str,
        args: Vec<TypedIdentifier<'a>>,
        returns: AzulaType<'a>,
        body: Rc<Statement<'a>>,
        span: Span,
    },
    Return(Option<ExpressionNode<'a>>, Span),
    Assign(
        bool,
        String,
        Option<AzulaType<'a>>,
        ExpressionNode<'a>,
        Span,
    ),
    ExpressionStatement(ExpressionNode<'a>, Span),
    If(ExpressionNode<'a>, Body<'a>, Option<Rc<Statement<'a>>>, Span),
    ExternFunction {
        name: &'a str,
        varargs: bool,
        args: Vec<AzulaType<'a>>,
        returns: AzulaType<'a>,
        span: Span,
    },
    Reassign(ExpressionNode<'a>, ExpressionNode<'a>, Span),
    While(ExpressionNode<'a>, Body<'a>, Span),
    For(Option<ExpressionNode<'a>>, Body<'a>, Span),
    /// `for name in iterable { }`, or `for name in start..end` (the second
    /// expression and whether the range includes its end)
    ForIn(String, ExpressionNode<'a>, Option<ExpressionNode<'a>>, bool, Body<'a>, Span),
    /// `var (a, _, c) = tuple;` (`_` ignores an element)
    Destructure(bool, Vec<String>, ExpressionNode<'a>, Span),
    /// Statements that share the enclosing scope (produced by the typechecker)
    Group(Body<'a>),
    /// `target op= value`
    CompoundAssign(ExpressionNode<'a>, Operator, ExpressionNode<'a>, Span),
    Break(Span),
    Continue(Span),
    Struct {
        name: &'a str,
        attributes: Vec<TypedIdentifier<'a>>,
        span: Span,
    },
    Impl {
        struct_impl: AzulaType<'a>,
        trait_impl: Option<AzulaType<'a>>,
        funcs: Vec<Statement<'a>>,
        span: Span,
    },
    Enum {
        name: &'a str,
        variants: Vec<&'a str>,
        /// The payload types carried by each variant (empty for plain variants)
        payloads: Vec<Vec<AzulaType<'a>>>,
        span: Span,
    },
    TypeAlias {
        name: &'a str,
        typ: AzulaType<'a>,
        span: Span,
    },
    Import(String, Span),
    /// A generic definition: a Function, Struct, Enum or Impl parameterised
    /// over the listed type parameters, with bounds (`T is Show` as ("T", "Show")).
    Generic(Vec<&'a str>, Vec<(&'a str, &'a str)>, Rc<Statement<'a>>),
    /// `interface Name { methods }`: each method, and whether it has a default body
    Interface {
        name: &'a str,
        methods: Vec<(Statement<'a>, bool)>,
        span: Span,
    },
    /// `Type is A, B`: the type implements these interfaces
    Conforms(AzulaType<'a>, Vec<&'a str>, Span),
}

#[derive(Debug, PartialEq, Clone)]
pub enum Expression<'a> {
    Infix(Rc<ExpressionNode<'a>>, Operator, Rc<ExpressionNode<'a>>),
    Integer(i64),
    Float(f64),
    Identifier(String),
    Boolean(bool),
    String(String),
    FunctionCall {
        function: Rc<ExpressionNode<'a>>,
        args: Vec<ExpressionNode<'a>>,
    },
    Not(Rc<ExpressionNode<'a>>),
    BitNot(Rc<ExpressionNode<'a>>),
    SizeOf(AzulaType<'a>),
    Negate(Rc<ExpressionNode<'a>>),
    Pointer(Rc<ExpressionNode<'a>>),
    /// `*pointer`
    Deref(Rc<ExpressionNode<'a>>),
    /// `"text ${expr} text"`: the pieces to convert to strings and join
    Interpolation(Vec<ExpressionNode<'a>>),
    Array(Vec<ExpressionNode<'a>>),
    /// `(a, b)`
    Tuple(Vec<ExpressionNode<'a>>),
    /// `value?`: the value of an Option or Result, or return early with its None or Err
    Try(Rc<ExpressionNode<'a>>),
    /// `if cond { a } else { b }` used as a value
    If(Rc<ExpressionNode<'a>>, Rc<ExpressionNode<'a>>, Option<Rc<ExpressionNode<'a>>>),
    /// `func(a: A, b) : R { body }` or `func(a) => value`: parameters (whose
    /// types may be left to inference), return type if given, and body
    Closure(Vec<(Option<AzulaType<'a>>, String)>, Option<AzulaType<'a>>, Vec<Statement<'a>>),
    /// A closure object for the lifted function, holding the given captured
    /// variable cells (produced by the typechecker)
    MakeClosure(String, Vec<ExpressionNode<'a>>),
    /// Cell `n` of the current closure's environment (produced by the typechecker)
    EnvCell(usize),
    /// A new heap cell holding a value: variables captured by closures live
    /// in these (produced by the typechecker)
    NewCell(Rc<ExpressionNode<'a>>),
    /// Call of a function value (produced by the typechecker)
    CallClosure(Rc<ExpressionNode<'a>>, Vec<ExpressionNode<'a>>),
    ArrayAccess(Rc<ExpressionNode<'a>>, Rc<ExpressionNode<'a>>),
    StructInitialisation(Rc<ExpressionNode<'a>>, Vec<(&'a str, ExpressionNode<'a>)>),
    StructAccess(Rc<ExpressionNode<'a>>, Rc<ExpressionNode<'a>>),
    NamespaceAccess(Rc<ExpressionNode<'a>>, Rc<ExpressionNode<'a>>),
    Match(
        Rc<ExpressionNode<'a>>,
        Vec<(MatchPattern<'a>, ExpressionNode<'a>)>,
    ),
    Cast(Rc<ExpressionNode<'a>>, AzulaType<'a>),
    Alloc(Rc<ExpressionNode<'a>>),
    Null,
    /// `Name::<Types>` — explicit type arguments for a generic function or type
    Turbofish(String, Vec<AzulaType<'a>>),
    Block(Vec<Statement<'a>>, Option<Rc<ExpressionNode<'a>>>),
}

#[derive(Debug, PartialEq, Clone)]
pub enum MatchPattern<'a> {
    /// EnumName::Variant
    Variant(&'a str, &'a str),
    /// EnumName::Variant(a, _, c) — binds the variant's payload fields (`_` ignores one)
    Destructure(&'a str, &'a str, Vec<Option<&'a str>>),
    /// integer or char literal
    Integer(i64),
    /// _
    Wildcard,
    /// A name binding a tuple element
    Binding(&'a str),
    /// `A | B`: any of the alternatives (which can't bind names)
    Or(Vec<MatchPattern<'a>>),
    /// `pattern if condition`
    Guarded(Rc<MatchPattern<'a>>, Rc<ExpressionNode<'a>>),
    /// (p, q, ...)
    Tuple(Vec<MatchPattern<'a>>),
}

#[derive(Debug, PartialEq, Clone)]
pub struct ExpressionNode<'a> {
    pub expression: Expression<'a>,
    pub typed: AzulaType<'a>,
    pub span: Span,
}

#[derive(Debug, PartialEq, Clone, Hash, Eq)]
pub enum Operator {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Power,
    Or,
    And,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

pub type Body<'a> = Vec<Statement<'a>>;
pub type TypedIdentifier<'a> = (AzulaType<'a>, &'a str);

// #[derive(Debug, PartialEq, Clone)]
// pub enum Type<'a> {
//     Basic(&'a str),
//     WithArgument(&'a str, Rc<Type<'a>>),
//     Pointer(Rc<Type<'a>>),
//     Infer,
//     None,
// }

#[derive(Debug, PartialEq, Clone)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}
