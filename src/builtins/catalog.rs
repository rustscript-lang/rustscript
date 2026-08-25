// Authoritative static builtin ID catalog.
//
// Every VM-visible builtin (ordinary, internal, and special-call) receives one
// explicit, immutable u16 call index assigned in this file. build.rs parses
// this file and generates the `BuiltinFunction` enum discriminants,
// `call_index`, `from_call_index`, and catalog iteration from these IDs.
// `pd-vm-nostd` consumes a checked-in generated mirror
// (pd-vm-nostd/src/generated_builtin_ids.rs), which the workspace test
// `static_builtin_ids_are_frozen` keeps in sync.
//
// # ID blocks (shared u16 call-index space)
//
// | Block | Range | Purpose |
// |---|---|---|
// | extension | 0x0000 ..= 0xFF8F | reserved for future builtins and host imports |
// | special-call | 0xFF90 ..= 0xFFA1 | special-call builtins (incl. internal lowering builtins) |
// | ordinary | 0xFFA2 ..= 0xFFFF | ordinary builtins (language + namespaced) |
//
// The top-u16 range 0xFFFC ..= 0xFFFF is reserved for the frozen SQLite
// assignments below. Do not allocate an ordinary ID by incrementing a u16
// cursor through this range: incrementing 0xFFFF would overflow, and these
// four IDs must remain stable even when SQLite is feature-disabled.
//
// # Rules
//
// - IDs are immutable once assigned. Appending or reordering entries must not
//   renumber existing entries.
// - build.rs fails the build on duplicate IDs, duplicate source names,
//   duplicate Rust variants, out-of-block IDs, a discovered runtime callable
//   without an explicit ID, or a catalog entry without a runtime callable.
// - Class is one of Ordinary | Internal | Special. Internal entries are the
//   `__`-prefixed lowering builtins; Special entries are the remaining
//   special-call builtins; both live in the special-call block.
// - The feature gate column names the cargo feature gating the runtime
//   implementation, or `none`.
//
// Entry syntax (parsed textually by build.rs):
//
//   builtin_id!(0xXXXX, "source_name", RustVariant, Class, feature_gate);
//
// source_name must equal the `#[pd_host_function(name = ...)]` value of the
// runtime callable, and RustVariant must equal the derived variant name.

builtin_id!(0xFFA2, "len", Len, Ordinary, none);
builtin_id!(0xFFA3, "slice", Slice, Ordinary, none);
builtin_id!(0xFFA4, "concat", Concat, Ordinary, none);
builtin_id!(0xFFA5, "array_new", ArrayNew, Ordinary, none);
builtin_id!(0xFFA6, "array_push", ArrayPush, Ordinary, none);
builtin_id!(0xFFA7, "map_new", MapNew, Ordinary, none);
builtin_id!(0xFFA8, "get", Get, Ordinary, none);
builtin_id!(0xFFA9, "has", Has, Ordinary, none);
builtin_id!(0xFFAA, "set", Set, Ordinary, none);
builtin_id!(0xFFAB, "keys", Keys, Ordinary, none);
builtin_id!(0xFFAC, "bytes::from_utf8", BytesFromUtf8, Ordinary, none);
builtin_id!(0xFFAD, "bytes::to_utf8", BytesToUtf8, Ordinary, none);
builtin_id!(0xFFAE, "bytes::to_utf8_lossy", BytesToUtf8Lossy, Ordinary, none);
builtin_id!(0xFFAF, "bytes::from_hex", BytesFromHex, Ordinary, none);
builtin_id!(0xFFB0, "bytes::to_hex", BytesToHex, Ordinary, none);
builtin_id!(0xFFB1, "bytes::from_base64", BytesFromBase64, Ordinary, none);
builtin_id!(0xFFB2, "bytes::to_base64", BytesToBase64, Ordinary, none);
builtin_id!(0xFFB3, "bytes::from_array_u8", BytesFromArrayU8, Ordinary, none);
builtin_id!(0xFFB4, "bytes::to_array_u8", BytesToArrayU8, Ordinary, none);
builtin_id!(0xFFB5, "io::open", IoOpen, Ordinary, none);
builtin_id!(0xFFB6, "io::popen", IoPopen, Ordinary, none);
builtin_id!(0xFFB7, "io::read_all", IoReadAll, Ordinary, none);
builtin_id!(0xFFB8, "io::read_line", IoReadLine, Ordinary, none);
builtin_id!(0xFFB9, "io::write", IoWrite, Ordinary, none);
builtin_id!(0xFFBA, "io::flush", IoFlush, Ordinary, none);
builtin_id!(0xFFBB, "io::close", IoClose, Ordinary, none);
builtin_id!(0xFFBC, "io::exists", IoExists, Ordinary, none);
builtin_id!(0xFFC3, "sqlite::open", SqliteOpen, Ordinary, none);
builtin_id!(0xFFFC, "sqlite::execute", SqliteExecute, Ordinary, none);
builtin_id!(0xFFFD, "sqlite::query", SqliteQuery, Ordinary, none);
builtin_id!(0xFFFE, "sqlite::transaction", SqliteTransaction, Ordinary, none);
builtin_id!(0xFFFF, "sqlite::close", SqliteClose, Ordinary, none);
builtin_id!(0xFFBD, "re::match", ReMatch, Ordinary, none);
builtin_id!(0xFFBE, "re::find", ReFind, Ordinary, none);
builtin_id!(0xFFBF, "re::replace", ReReplace, Ordinary, none);
builtin_id!(0xFFC0, "re::split", ReSplit, Ordinary, none);
builtin_id!(0xFFC1, "re::captures", ReCaptures, Ordinary, none);
builtin_id!(0xFFC2, "json::encode", JsonEncode, Ordinary, none);
builtin_id!(0xFFC4, "json::decode", JsonDecode, Ordinary, none);
builtin_id!(0xFFC5, "jit::set_config", JitSetConfig, Ordinary, none);
builtin_id!(0xFFC6, "jit::get_config", JitGetConfig, Ordinary, none);
builtin_id!(0xFFC7, "jit::set_enabled", JitSetEnabled, Ordinary, none);
builtin_id!(0xFFC8, "jit::get_enabled", JitGetEnabled, Ordinary, none);
builtin_id!(0xFFC9, "jit::set_hot_loop_threshold", JitSetHotLoopThreshold, Ordinary, none);
builtin_id!(0xFFCA, "jit::get_hot_loop_threshold", JitGetHotLoopThreshold, Ordinary, none);
builtin_id!(0xFFCB, "jit::set_max_trace_len", JitSetMaxTraceLen, Ordinary, none);
builtin_id!(0xFFCC, "jit::get_max_trace_len", JitGetMaxTraceLen, Ordinary, none);
builtin_id!(0xFFCD, "math::pi", MathPi, Ordinary, none);
builtin_id!(0xFFCE, "math::tau", MathTau, Ordinary, none);
builtin_id!(0xFFCF, "math::e", MathE, Ordinary, none);
builtin_id!(0xFFD0, "math::epsilon", MathEpsilon, Ordinary, none);
builtin_id!(0xFFD1, "math::inf", MathInf, Ordinary, none);
builtin_id!(0xFFD2, "math::neg_inf", MathNegInf, Ordinary, none);
builtin_id!(0xFFD3, "math::nan", MathNaN, Ordinary, none);
builtin_id!(0xFFD4, "math::abs", MathAbs, Ordinary, none);
builtin_id!(0xFFD5, "math::sqrt", MathSqrt, Ordinary, none);
builtin_id!(0xFFD6, "math::cbrt", MathCbrt, Ordinary, none);
builtin_id!(0xFFD7, "math::exp", MathExp, Ordinary, none);
builtin_id!(0xFFD8, "math::exp2", MathExp2, Ordinary, none);
builtin_id!(0xFFD9, "math::ln", MathLn, Ordinary, none);
builtin_id!(0xFFDA, "math::ln_1p", MathLn1p, Ordinary, none);
builtin_id!(0xFFDB, "math::log2", MathLog2, Ordinary, none);
builtin_id!(0xFFDC, "math::log10", MathLog10, Ordinary, none);
builtin_id!(0xFFDD, "math::sin", MathSin, Ordinary, none);
builtin_id!(0xFFDE, "math::cos", MathCos, Ordinary, none);
builtin_id!(0xFFDF, "math::tan", MathTan, Ordinary, none);
builtin_id!(0xFFE0, "math::asin", MathAsin, Ordinary, none);
builtin_id!(0xFFE1, "math::acos", MathAcos, Ordinary, none);
builtin_id!(0xFFE2, "math::atan", MathAtan, Ordinary, none);
builtin_id!(0xFFE3, "math::sinh", MathSinh, Ordinary, none);
builtin_id!(0xFFE4, "math::cosh", MathCosh, Ordinary, none);
builtin_id!(0xFFE5, "math::tanh", MathTanh, Ordinary, none);
builtin_id!(0xFFE6, "math::floor", MathFloor, Ordinary, none);
builtin_id!(0xFFE7, "math::ceil", MathCeil, Ordinary, none);
builtin_id!(0xFFE8, "math::round", MathRound, Ordinary, none);
builtin_id!(0xFFE9, "math::trunc", MathTrunc, Ordinary, none);
builtin_id!(0xFFEA, "math::fract", MathFract, Ordinary, none);
builtin_id!(0xFFEB, "math::signum", MathSignum, Ordinary, none);
builtin_id!(0xFFEC, "math::to_degrees", MathToDegrees, Ordinary, none);
builtin_id!(0xFFED, "math::to_radians", MathToRadians, Ordinary, none);
builtin_id!(0xFFEE, "math::is_nan", MathIsNaN, Ordinary, none);
builtin_id!(0xFFEF, "math::is_infinite", MathIsInfinite, Ordinary, none);
builtin_id!(0xFFF0, "math::is_finite", MathIsFinite, Ordinary, none);
builtin_id!(0xFFF1, "math::atan2", MathAtan2, Ordinary, none);
builtin_id!(0xFFF2, "math::powf", MathPowF, Ordinary, none);
builtin_id!(0xFFF3, "math::powi", MathPowI, Ordinary, none);
builtin_id!(0xFFF4, "math::hypot", MathHypot, Ordinary, none);
builtin_id!(0xFFF5, "math::log", MathLog, Ordinary, none);
builtin_id!(0xFFF6, "math::min", MathMin, Ordinary, none);
builtin_id!(0xFFF7, "math::max", MathMax, Ordinary, none);
builtin_id!(0xFFF8, "math::copysign", MathCopySign, Ordinary, none);
builtin_id!(0xFFF9, "math::clamp", MathClamp, Ordinary, none);
builtin_id!(0xFFFA, "math::mul_add", MathMulAdd, Ordinary, none);
builtin_id!(0xFFFB, "count", Count, Ordinary, none);
builtin_id!(0xFF9E, "__format_template", FormatTemplate, Internal, none);
builtin_id!(0xFF9F, "__to_string", ToString, Internal, none);
builtin_id!(0xFFA0, "type", TypeOf, Special, none);
builtin_id!(0xFFA1, "assert", Assert, Special, none);
builtin_id!(0xFF9B, "string_contains", StringContains, Special, none);
builtin_id!(0xFF9C, "string_replace_literal", StringReplaceLiteral, Special, none);
builtin_id!(0xFF9D, "string_lower_ascii", StringLowerAscii, Special, none);
builtin_id!(0xFF9A, "string_split_literal", StringSplitLiteral, Special, none);
builtin_id!(0xFF99, "__map_iter_init", MapIterInit, Internal, none);
builtin_id!(0xFF98, "__map_iter_next", MapIterNext, Internal, none);
builtin_id!(0xFF97, "__map_iter_take_key", MapIterTakeKey, Internal, none);
builtin_id!(0xFF96, "__map_iter_take_value", MapIterTakeValue, Internal, none);
builtin_id!(0xFF95, "__map_iter_close", MapIterClose, Internal, none);
builtin_id!(0xFF94, "__bind_callable", BindCallable, Internal, none);
builtin_id!(0xFF93, "__detach_local", DetachLocal, Internal, none);
