use super::*;
use std::hash::{Hash, Hasher};

pub(super) fn detect_native_stack_layout() -> VmResult<NativeStackLayout> {
    let cached = NATIVE_STACK_LAYOUT
        .get_or_init(|| detect_native_stack_layout_uncached().map_err(layout_probe_error_message));
    match cached {
        Ok(layout) => Ok(*layout),
        Err(message) => Err(VmError::JitNative(message.clone())),
    }
}

pub(super) fn native_layout_fingerprint() -> VmResult<u64> {
    let layout = detect_native_stack_layout()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();

    layout.vm_stack_offset.hash(&mut hasher);
    layout.vm_locals_offset.hash(&mut hasher);
    layout.vm_program_constants_ptr_offset.hash(&mut hasher);
    layout.vm_program_constants_len_offset.hash(&mut hasher);
    layout.vm_ip_offset.hash(&mut hasher);
    layout.vm_interrupt_mode_offset.hash(&mut hasher);
    layout.vm_fuel_remaining_offset.hash(&mut hasher);
    layout.vm_fuel_check_interval_offset.hash(&mut hasher);
    layout.vm_fuel_ops_until_check_offset.hash(&mut hasher);
    layout.vm_epoch_deadline_offset.hash(&mut hasher);
    layout.vm_epoch_counter_ptr_offset.hash(&mut hasher);
    layout.vm_drop_contract_events_offset.hash(&mut hasher);
    layout.stack_vec.ptr_offset.hash(&mut hasher);
    layout.stack_vec.len_offset.hash(&mut hasher);
    layout.stack_vec.cap_offset.hash(&mut hasher);
    layout.value.size.hash(&mut hasher);
    layout.value.tag_offset.hash(&mut hasher);
    layout.value.tag_size.hash(&mut hasher);
    layout.value.null_tag.hash(&mut hasher);
    layout.value.int_tag.hash(&mut hasher);
    layout.value.float_tag.hash(&mut hasher);
    layout.value.bool_tag.hash(&mut hasher);
    layout.value.int_payload_offset.hash(&mut hasher);
    layout.value.float_payload_offset.hash(&mut hasher);
    layout.value.bool_payload_offset.hash(&mut hasher);
    std::mem::offset_of!(Vm, native_helper_fn).hash(&mut hasher);

    Ok(hasher.finish())
}

fn detect_native_stack_layout_uncached() -> VmResult<NativeStackLayout> {
    let vm_stack_offset = usize_to_i32(std::mem::offset_of!(Vm, stack), "Vm::stack offset")?;
    let vm_locals_offset = usize_to_i32(std::mem::offset_of!(Vm, locals), "Vm::locals offset")?;
    let vm_program_constants_ptr_offset = usize_to_i32(
        std::mem::offset_of!(Vm, program_constants_ptr),
        "Vm::program_constants_ptr offset",
    )?;
    let vm_program_constants_len_offset = usize_to_i32(
        std::mem::offset_of!(Vm, program_constants_len),
        "Vm::program_constants_len offset",
    )?;
    let vm_ip_offset = usize_to_i32(std::mem::offset_of!(Vm, ip), "Vm::ip offset")?;
    let vm_interrupt_mode_offset = usize_to_i32(
        std::mem::offset_of!(Vm, interrupt_mode),
        "Vm::interrupt_mode offset",
    )?;
    let vm_fuel_remaining_offset = usize_to_i32(
        std::mem::offset_of!(Vm, fuel_remaining),
        "Vm::fuel_remaining offset",
    )?;
    let vm_fuel_check_interval_offset = usize_to_i32(
        std::mem::offset_of!(Vm, fuel_check_interval),
        "Vm::fuel_check_interval offset",
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
    let vm_drop_contract_events_offset = usize_to_i32(
        std::mem::offset_of!(Vm, drop_contract_events),
        "Vm::drop_contract_events offset",
    )?;
    let stack_vec = detect_vec_layout()?;
    let value = detect_value_layout()?;
    Ok(NativeStackLayout {
        vm_stack_offset,
        vm_locals_offset,
        vm_program_constants_ptr_offset,
        vm_program_constants_len_offset,
        vm_ip_offset,
        vm_interrupt_mode_offset,
        vm_fuel_remaining_offset,
        vm_fuel_check_interval_offset,
        vm_fuel_ops_until_check_offset,
        vm_epoch_deadline_offset,
        vm_epoch_counter_ptr_offset,
        vm_drop_contract_events_offset,
        stack_vec,
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
    let cap_value = sample.capacity();

    let words = unsafe { &*((&sample as *const Vec<Value>) as *const [usize; 3]) };
    let ptr_index = find_unique_word_index(words, ptr_value, "Vec<Value> ptr field")?;
    let len_index = find_unique_word_index(words, len_value, "Vec<Value> len field")?;
    let cap_index = find_unique_word_index(words, cap_value, "Vec<Value> cap field")?;

    Ok(VecLayout {
        ptr_offset: usize_to_i32(
            ptr_index * std::mem::size_of::<usize>(),
            "Vec<Value>::ptr offset",
        )?,
        len_offset: usize_to_i32(
            len_index * std::mem::size_of::<usize>(),
            "Vec<Value>::len offset",
        )?,
        cap_offset: usize_to_i32(
            cap_index * std::mem::size_of::<usize>(),
            "Vec<Value>::cap offset",
        )?,
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
    let null_a_bytes = encode_value_bytes(Value::Null);
    let null_b_bytes = encode_value_bytes(Value::Null);
    let int_a_bytes = encode_value_bytes(Value::Int(int_a));
    let int_b_bytes = encode_value_bytes(Value::Int(int_b));
    let float_a_bytes = encode_value_bytes(Value::Float(float_a));
    let float_b_bytes = encode_value_bytes(Value::Float(float_b));
    let bool_false_bytes = encode_value_bytes(Value::Bool(false));
    let bool_true_bytes = encode_value_bytes(Value::Bool(true));
    let string_a_bytes = encode_value_bytes(Value::string("a"));
    let string_b_bytes = encode_value_bytes(Value::string("b"));

    let stable_tag_pairs = [
        (&null_a_bytes[..], &null_b_bytes[..]),
        (&int_a_bytes[..], &int_b_bytes[..]),
        (&float_a_bytes[..], &float_b_bytes[..]),
        (&bool_false_bytes[..], &bool_true_bytes[..]),
        (&string_a_bytes[..], &string_b_bytes[..]),
    ];
    let (tag_offset, tag_size) = detect_tag_layout(&stable_tag_pairs)?;
    let null_tag = decode_tag(&null_a_bytes, tag_offset, tag_size);
    let int_tag = decode_tag(&int_a_bytes, tag_offset, tag_size);
    let float_tag = decode_tag(&float_a_bytes, tag_offset, tag_size);
    let bool_tag = decode_tag(&bool_false_bytes, tag_offset, tag_size);

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

    Ok(ValueLayout {
        size: usize_to_i32(value_size, "Value size")?,
        tag_offset: usize_to_i32(tag_offset, "Value tag offset")?,
        tag_size: tag_size as u8,
        null_tag,
        int_tag,
        float_tag,
        bool_tag,
        int_payload_offset: usize_to_i32(int_payload_offset, "Value::Int payload offset")?,
        float_payload_offset: usize_to_i32(float_payload_offset, "Value::Float payload offset")?,
        bool_payload_offset: usize_to_i32(bool_payload_offset, "Value::Bool payload offset")?,
    })
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

pub(super) fn checked_add_i32(lhs: i32, rhs: i32, context: &str) -> VmResult<i32> {
    lhs.checked_add(rhs)
        .ok_or_else(|| VmError::JitNative(context.to_string()))
}

fn usize_to_i32(value: usize, context: &str) -> VmResult<i32> {
    i32::try_from(value)
        .map_err(|_| VmError::JitNative(format!("{} exceeds 32-bit displacement range", context)))
}
