use std::sync::OnceLock;

use crate::bytecode::{VmMap, vm_map_len_field_offset};

use super::super::{Value, Vm, VmError, VmResult};

static NATIVE_STACK_LAYOUT: OnceLock<Result<NativeStackLayout, String>> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) struct VecLayout {
    pub(crate) ptr_offset: i32,
    pub(crate) len_offset: i32,
}

#[derive(Clone, Copy)]
pub(crate) struct ValueLayout {
    pub(crate) size: i32,
    pub(crate) tag_offset: i32,
    pub(crate) tag_size: u8,
    pub(crate) null_tag: u32,
    pub(crate) int_tag: u32,
    pub(crate) float_tag: u32,
    pub(crate) bool_tag: u32,
    pub(crate) string_tag: u32,
    pub(crate) bytes_tag: u32,
    pub(crate) array_tag: u32,
    pub(crate) map_tag: u32,
    pub(crate) int_payload_offset: i32,
    pub(crate) float_payload_offset: i32,
    pub(crate) bool_payload_offset: i32,
    pub(crate) heap_payload_offset: i32,
    pub(crate) arc_data_offset: i32,
}

#[derive(Clone, Copy)]
pub(crate) struct MapLayout {
    pub(crate) len_offset: i32,
}

#[derive(Clone, Copy)]
pub(crate) struct NativeStackLayout {
    pub(crate) vm_stack_offset: i32,
    pub(crate) vm_locals_offset: i32,
    pub(crate) vm_program_constants_ptr_offset: i32,
    pub(crate) vm_ip_offset: i32,
    pub(crate) vm_fuel_remaining_offset: i32,
    pub(crate) vm_fuel_ops_until_check_offset: i32,
    pub(crate) vm_epoch_deadline_offset: i32,
    pub(crate) vm_epoch_counter_ptr_offset: i32,
    pub(crate) stack_vec: VecLayout,
    pub(crate) map: MapLayout,
    pub(crate) value: ValueLayout,
}

pub(crate) fn detect_native_stack_layout() -> VmResult<NativeStackLayout> {
    let cached = NATIVE_STACK_LAYOUT
        .get_or_init(|| detect_native_stack_layout_uncached().map_err(layout_probe_error_message));
    match cached {
        Ok(layout) => Ok(*layout),
        Err(message) => Err(VmError::JitNative(message.clone())),
    }
}

fn detect_native_stack_layout_uncached() -> VmResult<NativeStackLayout> {
    let vm_stack_offset = usize_to_i32(std::mem::offset_of!(Vm, stack), "Vm::stack offset")?;
    let vm_locals_offset = usize_to_i32(std::mem::offset_of!(Vm, locals), "Vm::locals offset")?;
    let vm_program_constants_ptr_offset = usize_to_i32(
        std::mem::offset_of!(Vm, program_constants_ptr),
        "Vm::program_constants_ptr offset",
    )?;
    let vm_ip_offset = usize_to_i32(std::mem::offset_of!(Vm, ip), "Vm::ip offset")?;
    let vm_fuel_remaining_offset = usize_to_i32(
        std::mem::offset_of!(Vm, fuel_remaining),
        "Vm::fuel_remaining offset",
    )?;
    let vm_fuel_ops_until_check_offset = usize_to_i32(
        std::mem::offset_of!(Vm, fuel_ops_until_check),
        "Vm::fuel_ops_until_check offset",
    )?;
    let vm_epoch_deadline_offset = usize_to_i32(
        std::mem::offset_of!(Vm, epoch_deadline),
        "Vm::epoch_deadline offset",
    )?;
    let vm_epoch_counter_ptr_offset = usize_to_i32(
        std::mem::offset_of!(Vm, epoch_counter_ptr),
        "Vm::epoch_counter_ptr offset",
    )?;
    let stack_vec = detect_vec_layout()?;
    let map = detect_map_layout()?;
    let value = detect_value_layout()?;
    Ok(NativeStackLayout {
        vm_stack_offset,
        vm_locals_offset,
        vm_program_constants_ptr_offset,
        vm_ip_offset,
        vm_fuel_remaining_offset,
        vm_fuel_ops_until_check_offset,
        vm_epoch_deadline_offset,
        vm_epoch_counter_ptr_offset,
        stack_vec,
        map,
        value,
    })
}

fn layout_probe_error_message(error: VmError) -> String {
    match error {
        VmError::JitNative(message) => message,
        other => other.to_string(),
    }
}

fn detect_vec_layout() -> VmResult<VecLayout> {
    let expected_size = std::mem::size_of::<[usize; 3]>();
    if std::mem::size_of::<Vec<Value>>() != expected_size {
        return Err(VmError::JitNative(format!(
            "unsupported Vec<Value> size {} for native emission",
            std::mem::size_of::<Vec<Value>>()
        )));
    }

    let mut sample = Vec::with_capacity(11);
    sample.push(Value::Int(1));
    sample.push(Value::Int(2));
    let ptr_value = sample.as_ptr() as usize;
    let len_value = sample.len();
    let words = unsafe { &*((&sample as *const Vec<Value>) as *const [usize; 3]) };
    let ptr_index = find_unique_word_index(words, ptr_value, "Vec<Value> ptr field")?;
    let len_index = find_unique_word_index(words, len_value, "Vec<Value> len field")?;

    Ok(VecLayout {
        ptr_offset: usize_to_i32(
            ptr_index * std::mem::size_of::<usize>(),
            "Vec<Value>::ptr offset",
        )?,
        len_offset: usize_to_i32(
            len_index * std::mem::size_of::<usize>(),
            "Vec<Value>::len offset",
        )?,
    })
}

fn detect_map_layout() -> VmResult<MapLayout> {
    Ok(MapLayout {
        len_offset: usize_to_i32(vm_map_len_field_offset(), "VmMap::cached_len offset")?,
    })
}

fn find_unique_word_index(words: &[usize; 3], needle: usize, label: &str) -> VmResult<usize> {
    let mut match_index = None;
    for (index, value) in words.iter().enumerate() {
        if *value == needle {
            if match_index.is_some() {
                return Err(VmError::JitNative(format!(
                    "ambiguous {} while probing native layout",
                    label
                )));
            }
            match_index = Some(index);
        }
    }
    match_index.ok_or_else(|| {
        VmError::JitNative(format!(
            "failed to locate {} while probing native layout",
            label
        ))
    })
}

fn detect_value_layout() -> VmResult<ValueLayout> {
    let value_size = std::mem::size_of::<Value>();
    let int_a = 0x0102_0304_0506_0708_i64;
    let int_b = 0x1112_1314_1516_1718_i64;
    let float_a = 3.25_f64;
    let float_b = -11.5_f64;
    let string_a = std::sync::Arc::new(String::from("a"));
    let string_b = std::sync::Arc::new(String::from("b"));
    let bytes_a = std::sync::Arc::new(vec![1u8, 2, 3]);
    let bytes_b = std::sync::Arc::new(vec![4u8, 5, 6]);
    let array_a = std::sync::Arc::new(vec![Value::Int(1), Value::Int(2)]);
    let array_b = std::sync::Arc::new(vec![Value::Int(3), Value::Int(4)]);
    let map_a = std::sync::Arc::new(VmMap::from_entries(vec![(
        Value::string("left"),
        Value::Int(1),
    )]));
    let map_b = std::sync::Arc::new(VmMap::from_entries(vec![(
        Value::string("right"),
        Value::Int(2),
    )]));
    let null_a_bytes = encode_value_bytes(Value::Null);
    let null_b_bytes = encode_value_bytes(Value::Null);
    let int_a_bytes = encode_value_bytes(Value::Int(int_a));
    let int_b_bytes = encode_value_bytes(Value::Int(int_b));
    let float_a_bytes = encode_value_bytes(Value::Float(float_a));
    let float_b_bytes = encode_value_bytes(Value::Float(float_b));
    let bool_false_bytes = encode_value_bytes(Value::Bool(false));
    let bool_true_bytes = encode_value_bytes(Value::Bool(true));
    let string_a_bytes = encode_value_bytes(Value::String(string_a.clone()));
    let string_b_bytes = encode_value_bytes(Value::String(string_b.clone()));
    let bytes_a_bytes = encode_value_bytes(Value::Bytes(bytes_a.clone()));
    let bytes_b_bytes = encode_value_bytes(Value::Bytes(bytes_b.clone()));
    let array_a_bytes = encode_value_bytes(Value::Array(array_a.clone()));
    let array_b_bytes = encode_value_bytes(Value::Array(array_b.clone()));
    let map_a_bytes = encode_value_bytes(Value::Map(map_a.clone()));
    let map_b_bytes = encode_value_bytes(Value::Map(map_b.clone()));

    let stable_tag_pairs = [
        (&null_a_bytes[..], &null_b_bytes[..]),
        (&int_a_bytes[..], &int_b_bytes[..]),
        (&float_a_bytes[..], &float_b_bytes[..]),
        (&bool_false_bytes[..], &bool_true_bytes[..]),
        (&string_a_bytes[..], &string_b_bytes[..]),
        (&bytes_a_bytes[..], &bytes_b_bytes[..]),
        (&array_a_bytes[..], &array_b_bytes[..]),
        (&map_a_bytes[..], &map_b_bytes[..]),
    ];
    let (tag_offset, tag_size) = detect_tag_layout(&stable_tag_pairs)?;
    let null_tag = decode_tag(&null_a_bytes, tag_offset, tag_size);
    let int_tag = decode_tag(&int_a_bytes, tag_offset, tag_size);
    let float_tag = decode_tag(&float_a_bytes, tag_offset, tag_size);
    let bool_tag = decode_tag(&bool_false_bytes, tag_offset, tag_size);
    let string_tag = decode_tag(&string_a_bytes, tag_offset, tag_size);
    let bytes_tag = decode_tag(&bytes_a_bytes, tag_offset, tag_size);
    let array_tag = decode_tag(&array_a_bytes, tag_offset, tag_size);
    let map_tag = decode_tag(&map_a_bytes, tag_offset, tag_size);

    let payload_match_a = int_a.to_le_bytes();
    let payload_match_b = int_b.to_le_bytes();
    let mut int_payload_offset = None;
    for offset in 0..=value_size.saturating_sub(8) {
        if int_a_bytes[offset..offset + 8] == payload_match_a
            && int_b_bytes[offset..offset + 8] == payload_match_b
        {
            if int_payload_offset.is_some() {
                return Err(VmError::JitNative(
                    "ambiguous Value::Int payload offset for native emission".to_string(),
                ));
            }
            int_payload_offset = Some(offset);
        }
    }
    let int_payload_offset = int_payload_offset.ok_or_else(|| {
        VmError::JitNative(
            "unable to find Value::Int payload offset for native emission".to_string(),
        )
    })?;

    let float_payload_match_a = float_a.to_bits().to_le_bytes();
    let float_payload_match_b = float_b.to_bits().to_le_bytes();
    let mut float_payload_offset = None;
    for offset in 0..=value_size.saturating_sub(8) {
        if float_a_bytes[offset..offset + 8] == float_payload_match_a
            && float_b_bytes[offset..offset + 8] == float_payload_match_b
        {
            if float_payload_offset.is_some() {
                return Err(VmError::JitNative(
                    "ambiguous Value::Float payload offset for native emission".to_string(),
                ));
            }
            float_payload_offset = Some(offset);
        }
    }
    let float_payload_offset = float_payload_offset.ok_or_else(|| {
        VmError::JitNative(
            "unable to find Value::Float payload offset for native emission".to_string(),
        )
    })?;

    let mut bool_payload_offset = None;
    for offset in 0..value_size {
        if bool_false_bytes[offset] == bool_true_bytes[offset] {
            continue;
        }
        if offset >= tag_offset && offset < tag_offset + tag_size {
            continue;
        }
        bool_payload_offset = Some(offset);
        break;
    }
    let bool_payload_offset = bool_payload_offset.ok_or_else(|| {
        VmError::JitNative(
            "unable to find Value::Bool payload offset for native emission".to_string(),
        )
    })?;
    let false_byte = bool_false_bytes[bool_payload_offset];
    let true_byte = bool_true_bytes[bool_payload_offset];
    if false_byte != 0 || true_byte != 1 {
        return Err(VmError::JitNative(
            "unsupported Value::Bool byte encoding for native emission".to_string(),
        ));
    }

    let heap_payload_offset = detect_heap_payload_offset(
        value_size,
        &[
            (&string_a_bytes, arc_repr_word(&string_a)),
            (&string_b_bytes, arc_repr_word(&string_b)),
            (&bytes_a_bytes, arc_repr_word(&bytes_a)),
            (&bytes_b_bytes, arc_repr_word(&bytes_b)),
            (&array_a_bytes, arc_repr_word(&array_a)),
            (&array_b_bytes, arc_repr_word(&array_b)),
            (&map_a_bytes, arc_repr_word(&map_a)),
            (&map_b_bytes, arc_repr_word(&map_b)),
        ],
    )?;
    let arc_data_offset = detect_arc_data_offset(&[
        (
            arc_repr_word(&string_a),
            std::sync::Arc::as_ptr(&string_a) as usize,
        ),
        (
            arc_repr_word(&string_b),
            std::sync::Arc::as_ptr(&string_b) as usize,
        ),
        (
            arc_repr_word(&bytes_a),
            std::sync::Arc::as_ptr(&bytes_a) as usize,
        ),
        (
            arc_repr_word(&bytes_b),
            std::sync::Arc::as_ptr(&bytes_b) as usize,
        ),
        (
            arc_repr_word(&array_a),
            std::sync::Arc::as_ptr(&array_a) as usize,
        ),
        (
            arc_repr_word(&array_b),
            std::sync::Arc::as_ptr(&array_b) as usize,
        ),
        (
            arc_repr_word(&map_a),
            std::sync::Arc::as_ptr(&map_a) as usize,
        ),
        (
            arc_repr_word(&map_b),
            std::sync::Arc::as_ptr(&map_b) as usize,
        ),
    ])?;

    Ok(ValueLayout {
        size: usize_to_i32(value_size, "Value size")?,
        tag_offset: usize_to_i32(tag_offset, "Value tag offset")?,
        tag_size: tag_size as u8,
        null_tag,
        int_tag,
        float_tag,
        bool_tag,
        string_tag,
        bytes_tag,
        array_tag,
        map_tag,
        int_payload_offset: usize_to_i32(int_payload_offset, "Value::Int payload offset")?,
        float_payload_offset: usize_to_i32(float_payload_offset, "Value::Float payload offset")?,
        bool_payload_offset: usize_to_i32(bool_payload_offset, "Value::Bool payload offset")?,
        heap_payload_offset: usize_to_i32(heap_payload_offset, "Value heap payload offset")?,
        arc_data_offset: usize_to_i32(arc_data_offset, "Arc data offset")?,
    })
}

fn detect_heap_payload_offset(value_size: usize, samples: &[(&[u8], usize)]) -> VmResult<usize> {
    let pointer_size = std::mem::size_of::<usize>();
    let mut payload_offset = None;

    for offset in 0..=value_size.saturating_sub(pointer_size) {
        let matches = samples
            .iter()
            .all(|(bytes, ptr)| bytes[offset..offset + pointer_size] == ptr.to_ne_bytes());
        if !matches {
            continue;
        }
        if payload_offset.is_some() {
            return Err(VmError::JitNative(
                "ambiguous heap payload offset for native emission".to_string(),
            ));
        }
        payload_offset = Some(offset);
    }

    payload_offset.ok_or_else(|| {
        VmError::JitNative("unable to find heap payload offset for native emission".to_string())
    })
}

fn detect_arc_data_offset(samples: &[(usize, usize)]) -> VmResult<usize> {
    let mut data_offset = None;
    for (arc_word, data_ptr) in samples {
        let offset = data_ptr.checked_sub(*arc_word).ok_or_else(|| {
            VmError::JitNative("Arc data pointer precedes Arc storage word".to_string())
        })?;
        if let Some(existing) = data_offset {
            if existing != offset {
                return Err(VmError::JitNative(
                    "inconsistent Arc data offset for native emission".to_string(),
                ));
            }
        } else {
            data_offset = Some(offset);
        }
    }
    data_offset.ok_or_else(|| {
        VmError::JitNative("unable to detect Arc data offset for native emission".to_string())
    })
}

fn arc_repr_word<T>(value: &std::sync::Arc<T>) -> usize {
    debug_assert_eq!(
        std::mem::size_of::<std::sync::Arc<T>>(),
        std::mem::size_of::<usize>()
    );
    unsafe { *(&raw const *value as *const usize) }
}

fn detect_tag_layout(stable_pairs: &[(&[u8], &[u8])]) -> VmResult<(usize, usize)> {
    if stable_pairs.len() < 2 {
        return Err(VmError::JitNative(
            "need at least two value variants to detect native tag layout".to_string(),
        ));
    }
    let size = stable_pairs[0].0.len();
    for (lhs, rhs) in stable_pairs {
        if lhs.len() != size || rhs.len() != size {
            return Err(VmError::JitNative(
                "value byte probes must all have matching lengths".to_string(),
            ));
        }
    }

    for tag_size in [1usize, 2, 4] {
        if tag_size > size {
            continue;
        }
        for offset in 0..=size - tag_size {
            let mut all_stable = true;
            let mut first_tag_slice: Option<&[u8]> = None;
            let mut all_equal_across_variants = true;
            for (lhs, rhs) in stable_pairs {
                let lhs_slice = &lhs[offset..offset + tag_size];
                let rhs_slice = &rhs[offset..offset + tag_size];
                if lhs_slice != rhs_slice {
                    all_stable = false;
                    break;
                }
                if let Some(first) = first_tag_slice {
                    if lhs_slice != first {
                        all_equal_across_variants = false;
                    }
                } else {
                    first_tag_slice = Some(lhs_slice);
                }
            }
            if !all_stable || all_equal_across_variants {
                continue;
            }
            return Ok((offset, tag_size));
        }
    }
    Err(VmError::JitNative(
        "unable to find Value discriminant bytes for native emission".to_string(),
    ))
}

fn decode_tag(bytes: &[u8], offset: usize, size: usize) -> u32 {
    let mut out = 0u32;
    for index in 0..size {
        out |= (bytes[offset + index] as u32) << (index * 8);
    }
    out
}

fn encode_value_bytes(value: Value) -> Vec<u8> {
    let size = std::mem::size_of::<Value>();
    let mut bytes = vec![0u8; size];
    let mut slot = std::mem::MaybeUninit::<Value>::zeroed();
    unsafe {
        slot.as_mut_ptr().write(value);
        std::ptr::copy_nonoverlapping(slot.as_ptr() as *const u8, bytes.as_mut_ptr(), size);
        std::ptr::drop_in_place(slot.as_mut_ptr());
    }
    bytes
}

pub(crate) fn checked_add_i32(lhs: i32, rhs: i32, context: &str) -> VmResult<i32> {
    lhs.checked_add(rhs)
        .ok_or_else(|| VmError::JitNative(context.to_string()))
}

fn usize_to_i32(value: usize, context: &str) -> VmResult<i32> {
    i32::try_from(value)
        .map_err(|_| VmError::JitNative(format!("{} exceeds 32-bit displacement range", context)))
}
