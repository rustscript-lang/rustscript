include!("io.namespace.rs");
include!("regex.namespace.rs");
include!("json.namespace.rs");
include!("jit.namespace.rs");
include!("math.namespace.rs");

macro_rules! declare_all_builtin_namespaces {
    ($callback:ident) => {
        declare_io_namespace!($callback)
        declare_regex_namespace!($callback)
        declare_json_namespace!($callback)
        declare_jit_namespace!($callback)
        declare_math_namespace!($callback)
    };
}
