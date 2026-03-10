use super::*;

impl AvailabilityAnalyzer {
    pub(super) fn extract_moved_field_access(
        &self,
        call_index: u16,
        args: &[Expr],
    ) -> Option<(LocalSlot, MovedFieldKey)> {
        match BuiltinFunction::from_call_index(call_index)? {
            BuiltinFunction::Get => {
                if args.len() != 2 {
                    return None;
                }
                let Expr::Var(root_slot) = args.first()? else {
                    return None;
                };
                let key = self.extract_moved_field_key_for_access(args.get(1)?);
                Some((*root_slot, key))
            }
            BuiltinFunction::Slice => {
                if args.len() != 3 {
                    return None;
                }
                let Expr::Var(root_slot) = args.first()? else {
                    return None;
                };
                Some((*root_slot, MovedFieldKey::Slice))
            }
            _ => None,
        }
    }

    pub(super) fn extract_set_field_write_with_value<'a>(
        &self,
        expr: &'a Expr,
    ) -> Option<(LocalSlot, MovedFieldKey, &'a Expr)> {
        let Expr::Call(index, args) = expr else {
            return None;
        };
        if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Set) {
            return None;
        }
        if args.len() != 3 {
            return None;
        }
        let Expr::Var(root_slot) = args.first()? else {
            return None;
        };
        let key = self.extract_literal_moved_field_key(args.get(1)?)?;
        let value = args.get(2)?;
        Some((*root_slot, key, value))
    }

    pub(super) fn extract_collection_mutation_root(
        &self,
        call_index: u16,
        args: &[Expr],
    ) -> Option<LocalSlot> {
        let builtin = BuiltinFunction::from_call_index(call_index)?;
        let expected_arity = match builtin {
            BuiltinFunction::Set => 3,
            BuiltinFunction::ArrayPush => 2,
            _ => return None,
        };
        if args.len() != expected_arity {
            return None;
        }
        let Expr::Var(root_slot) = args.first()? else {
            return None;
        };
        Some(*root_slot)
    }

    pub(super) fn extract_literal_moved_field_key(&self, expr: &Expr) -> Option<MovedFieldKey> {
        match expr {
            Expr::String(value) => Some(MovedFieldKey::String(value.clone())),
            Expr::Int(value) => Some(MovedFieldKey::Index(*value)),
            _ => None,
        }
    }

    pub(super) fn extract_moved_field_key_for_access(&self, expr: &Expr) -> MovedFieldKey {
        match expr {
            Expr::String(value) => MovedFieldKey::String(value.clone()),
            Expr::Int(value) => MovedFieldKey::Index(*value),
            _ => MovedFieldKey::Dynamic,
        }
    }

    pub(super) fn handle_local_rebind_field_moves(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        expr: &Expr,
    ) {
        if let Some((root_slot, key, value_expr)) = self.extract_set_field_write_with_value(expr)
            && root_slot == target
        {
            self.mark_field_available(state, target, &key);
            if self.is_definitely_copyable_expr(value_expr, state) {
                self.mark_copyable_field(state, target, &key);
            } else {
                self.clear_copyable_field(state, target, &key);
            }
            return;
        }

        if let Expr::Var(source) = expr {
            self.copy_local_field_moves(state, *source, target);
            return;
        }

        self.clear_local_field_moves(state, target);
        if let Some(keys) = self.collect_copyable_fields_for_expr(expr, state) {
            self.set_local_copyable_fields(state, target, &keys);
        } else {
            self.clear_local_copyable_fields(state, target);
        }
    }

    pub(super) fn handle_local_rebind_collection_aliases(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        expr: &Expr,
    ) {
        if let Expr::Var(source) = expr {
            self.copy_local_collection_aliases(state, *source, target);
            return;
        }
        if let Expr::Call(index, args) = expr
            && let Some(param_index) = self.collection_passthrough_params.get(index).copied()
            && let Some(source_expr) = args.get(param_index)
            && self.is_definitely_collection_expr(source_expr, state)
        {
            if let Some(source_slot) = self.extract_collection_alias_local(source_expr) {
                self.copy_local_collection_aliases(state, source_slot, target);
            } else {
                self.set_local_collection_aliases(state, target, self.fresh_collection_aliases());
            }
            return;
        }
        match expr {
            Expr::ToOwned(inner) if self.is_definitely_collection_expr(inner, state) => {
                self.set_local_collection_aliases(state, target, self.fresh_collection_aliases());
                return;
            }
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                if let Expr::Var(source) = inner.as_ref() {
                    self.copy_local_collection_aliases(state, *source, target);
                    return;
                }
                if self.is_definitely_collection_expr(inner, state) {
                    self.set_local_collection_aliases(
                        state,
                        target,
                        self.fresh_collection_aliases(),
                    );
                    return;
                }
            }
            _ => {}
        }
        if self.is_definitely_collection_expr(expr, state) {
            self.set_local_collection_aliases(state, target, self.fresh_collection_aliases());
            return;
        }
        self.clear_local_collection_aliases(state, target);
    }

    pub(super) fn extract_collection_alias_local(&self, expr: &Expr) -> Option<LocalSlot> {
        match expr {
            Expr::Var(slot) => Some(*slot),
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                if let Expr::Var(slot) = inner.as_ref() {
                    Some(*slot)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub(super) fn copy_local_field_moves(
        &self,
        state: &mut FlowState,
        source: LocalSlot,
        target: LocalSlot,
    ) {
        if source == target {
            return;
        }
        self.clear_local_field_moves(state, target);
        self.clear_local_copyable_fields(state, target);
        if !self.is_trackable_local(target) {
            return;
        }

        let definite_from_source = state
            .moved_definite
            .iter()
            .filter(|entry| entry.root == source)
            .cloned()
            .collect::<Vec<_>>();
        for mut entry in definite_from_source {
            entry.root = target;
            state.moved_definite.insert(entry);
        }

        let possible_from_source = state
            .moved_possible
            .iter()
            .filter(|entry| entry.root == source)
            .cloned()
            .collect::<Vec<_>>();
        for mut entry in possible_from_source {
            entry.root = target;
            state.moved_possible.insert(entry);
        }

        let copyable_from_source = state
            .copyable_fields
            .iter()
            .filter(|entry| entry.root == source)
            .cloned()
            .collect::<Vec<_>>();
        for mut entry in copyable_from_source {
            entry.root = target;
            state.copyable_fields.insert(entry);
        }
    }

    pub(super) fn copy_local_collection_aliases(
        &self,
        state: &mut FlowState,
        source: LocalSlot,
        target: LocalSlot,
    ) {
        let source_slot = source as usize;
        let target_slot = target as usize;
        if source_slot >= self.local_count || target_slot >= self.local_count {
            return;
        }
        if source_slot == target_slot {
            return;
        }
        state.collection_aliases[target_slot] = state.collection_aliases[source_slot].clone();
    }

    pub(super) fn clear_local_field_moves(&self, state: &mut FlowState, target: LocalSlot) {
        state.moved_definite.retain(|entry| entry.root != target);
        state.moved_possible.retain(|entry| entry.root != target);
    }

    pub(super) fn clear_local_copyable_fields(&self, state: &mut FlowState, target: LocalSlot) {
        state.copyable_fields.retain(|entry| entry.root != target);
    }

    pub(super) fn clear_local_collection_aliases(&self, state: &mut FlowState, target: LocalSlot) {
        let slot = target as usize;
        if slot < self.local_count {
            state.collection_aliases[slot].clear();
        }
    }

    pub(super) fn set_local_copyable_fields(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        keys: &HashSet<MovedFieldKey>,
    ) {
        self.clear_local_copyable_fields(state, target);
        if !self.is_trackable_local(target) {
            return;
        }
        for key in keys {
            state.copyable_fields.insert(MovedFieldPath {
                root: target,
                key: key.clone(),
            });
        }
    }

    pub(super) fn set_local_collection_aliases(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        aliases: HashSet<u32>,
    ) {
        let slot = target as usize;
        if slot < self.local_count {
            state.collection_aliases[slot] = aliases;
        }
    }

    pub(super) fn fresh_collection_aliases(&self) -> HashSet<u32> {
        let mut out = HashSet::with_capacity(1);
        let alias_id = self.next_collection_alias_id.get();
        let next = alias_id
            .checked_add(1)
            .expect("collection alias id overflow");
        self.next_collection_alias_id.set(next);
        out.insert(alias_id);
        out
    }

    pub(super) fn mark_field_moved(
        &self,
        state: &mut FlowState,
        root: LocalSlot,
        key: MovedFieldKey,
    ) {
        if !self.is_trackable_local(root) {
            return;
        }
        let path = MovedFieldPath {
            root,
            key: key.clone(),
        };
        state.moved_definite.insert(path);
        state.moved_possible.insert(MovedFieldPath { root, key });
    }

    pub(super) fn mark_field_available(
        &self,
        state: &mut FlowState,
        root: LocalSlot,
        key: &MovedFieldKey,
    ) {
        let path = MovedFieldPath {
            root,
            key: key.clone(),
        };
        state.moved_definite.remove(&path);
        state.moved_possible.remove(&path);
    }

    pub(super) fn mark_copyable_field(
        &self,
        state: &mut FlowState,
        root: LocalSlot,
        key: &MovedFieldKey,
    ) {
        if !self.is_trackable_local(root) {
            return;
        }
        state.copyable_fields.insert(MovedFieldPath {
            root,
            key: key.clone(),
        });
    }

    pub(super) fn clear_copyable_field(
        &self,
        state: &mut FlowState,
        root: LocalSlot,
        key: &MovedFieldKey,
    ) {
        state.copyable_fields.remove(&MovedFieldPath {
            root,
            key: key.clone(),
        });
    }

    pub(super) fn is_copyable_field(
        &self,
        root: LocalSlot,
        key: &MovedFieldKey,
        state: &FlowState,
    ) -> bool {
        state.copyable_fields.contains(&MovedFieldPath {
            root,
            key: key.clone(),
        })
    }

    pub(super) fn moved_possible_for_root<'a>(
        &'a self,
        state: &'a FlowState,
        root: LocalSlot,
    ) -> impl Iterator<Item = &'a MovedFieldPath> {
        state
            .moved_possible
            .iter()
            .filter(move |entry| entry.root == root)
    }

    pub(super) fn moved_field_keys_conflict(lhs: &MovedFieldKey, rhs: &MovedFieldKey) -> bool {
        match (lhs, rhs) {
            (MovedFieldKey::Dynamic, _)
            | (_, MovedFieldKey::Dynamic)
            | (MovedFieldKey::Slice, _)
            | (_, MovedFieldKey::Slice) => true,
            (MovedFieldKey::String(lhs), MovedFieldKey::String(rhs)) => lhs == rhs,
            (MovedFieldKey::Index(lhs), MovedFieldKey::Index(rhs)) => lhs == rhs,
            _ => false,
        }
    }

    pub(super) fn require_field_available(
        &self,
        root: LocalSlot,
        key: &MovedFieldKey,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        if !self.is_trackable_local(root) {
            return Ok(());
        }
        if !self
            .moved_possible_for_root(state, root)
            .any(|entry| Self::moved_field_keys_conflict(&entry.key, key))
        {
            return Ok(());
        }
        let local_name = self
            .local_names
            .get(&root)
            .cloned()
            .unwrap_or_else(|| format!("#{root}"));
        let field_display = self.format_field_display(&local_name, key);
        Err(ParseError {
            span: None,
            code: Some("E_FIELD_MOVED".to_string()),
            line: line as usize,
            message: format!(
                "field '{field_display}' was moved earlier; use '{field_display}.copy()' to copy it before moving"
            ),
        })
    }

    pub(super) fn require_collection_mutation_permitted(
        &self,
        root: LocalSlot,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        let root_slot = root as usize;
        if root_slot >= self.local_count {
            return Ok(());
        }
        let root_aliases = &state.collection_aliases[root_slot];
        if root_aliases.is_empty() {
            return Ok(());
        }
        let conflict = (0..self.local_count).find(|other_slot| {
            if *other_slot == root_slot || !state.possible[*other_slot] {
                return false;
            }
            !state.collection_aliases[*other_slot].is_empty()
                && state.collection_aliases[*other_slot]
                    .intersection(root_aliases)
                    .next()
                    .is_some()
        });
        let Some(conflict_slot) = conflict else {
            return Ok(());
        };
        let root_name = self.display_local_name(root);
        let alias_name = self.display_local_name(conflict_slot as LocalSlot);
        Err(ParseError {
            span: None,
            code: Some("E_MUTATE_ALIASED_COLLECTION".to_string()),
            line: line as usize,
            message: format!(
                "cannot mutate local '{root_name}' while aliased by '{alias_name}'; detach one side with '.copy()' first"
            ),
        })
    }

    pub(super) fn format_field_display(&self, local_name: &str, key: &MovedFieldKey) -> String {
        match key {
            MovedFieldKey::String(value) => {
                if is_simple_ident(value) {
                    format!("{local_name}.{value}")
                } else {
                    format!("{local_name}[\"{value}\"]")
                }
            }
            MovedFieldKey::Index(value) => format!("{local_name}[{value}]"),
            MovedFieldKey::Dynamic => format!("{local_name}[<dynamic>]"),
            MovedFieldKey::Slice => format!("{local_name}[..]"),
        }
    }

    pub(super) fn is_trackable_local(&self, index: LocalSlot) -> bool {
        (index as usize) < self.local_count && self.local_names.contains_key(&index)
    }

    pub(super) fn display_local_name(&self, index: LocalSlot) -> String {
        self.local_names
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("#{index}"))
    }

    pub(super) fn collect_copyable_fields_for_expr(
        &self,
        expr: &Expr,
        state: &FlowState,
    ) -> Option<HashSet<MovedFieldKey>> {
        let Expr::Call(index, args) = expr else {
            return None;
        };
        let builtin = BuiltinFunction::from_call_index(*index)?;
        match builtin {
            BuiltinFunction::MapNew if args.is_empty() => Some(HashSet::new()),
            BuiltinFunction::Set if args.len() == 3 => {
                let mut keys = self.collect_copyable_fields_for_expr(&args[0], state)?;
                let key = self.extract_literal_moved_field_key(&args[1])?;
                if self.is_definitely_copyable_expr(&args[2], state) {
                    keys.insert(key);
                } else {
                    keys.remove(&key);
                }
                Some(keys)
            }
            _ => None,
        }
    }

    pub(super) fn is_definitely_copyable_expr(&self, expr: &Expr, state: &FlowState) -> bool {
        match expr {
            // RustScript models string values as move-by-default. String-specific ergonomics
            // (for example `p.a + p.a`) are handled by `analyze_expr_to_owned`.
            Expr::Null | Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) => true,
            Expr::Neg(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => self.is_definitely_copyable_expr(inner, state),
            Expr::Not(_)
            | Expr::And(_, _)
            | Expr::Or(_, _)
            | Expr::Eq(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _) => true,
            Expr::Add(lhs, rhs)
            | Expr::Sub(lhs, rhs)
            | Expr::Mul(lhs, rhs)
            | Expr::Div(lhs, rhs)
            | Expr::Mod(lhs, rhs) => {
                self.is_definitely_copyable_expr(lhs, state)
                    && self.is_definitely_copyable_expr(rhs, state)
            }
            Expr::Call(index, args) => self
                .extract_moved_field_access(*index, args)
                .map(|(root_slot, field_key)| self.is_copyable_field(root_slot, &field_key, state))
                .unwrap_or(false),
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.is_definitely_copyable_expr(then_expr, state)
                    && self.is_definitely_copyable_expr(else_expr, state)
            }
            Expr::Match { arms, default, .. } => {
                arms.iter()
                    .all(|(_, arm_expr)| self.is_definitely_copyable_expr(arm_expr, state))
                    && self.is_definitely_copyable_expr(default, state)
            }
            Expr::Var(index) => state
                .copyable_locals
                .get(*index as usize)
                .copied()
                .unwrap_or(false),
            _ => false,
        }
    }

    pub(super) fn is_definitely_collection_expr(&self, expr: &Expr, state: &FlowState) -> bool {
        match expr {
            Expr::Var(index) => state
                .collection_aliases
                .get(*index as usize)
                .is_some_and(|aliases| !aliases.is_empty()),
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.is_definitely_collection_expr(inner, state)
            }
            Expr::Call(index, args) => match BuiltinFunction::from_call_index(*index) {
                Some(BuiltinFunction::MapNew) => args.is_empty(),
                Some(BuiltinFunction::ArrayNew) => args.is_empty(),
                Some(BuiltinFunction::Set) if args.len() == 3 => {
                    self.is_definitely_collection_expr(&args[0], state)
                }
                Some(BuiltinFunction::ArrayPush) if args.len() == 2 => {
                    self.is_definitely_collection_expr(&args[0], state)
                }
                _ => false,
            },
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.is_definitely_collection_expr(then_expr, state)
                    && self.is_definitely_collection_expr(else_expr, state)
            }
            Expr::Match { arms, default, .. } => {
                arms.iter()
                    .all(|(_, arm_expr)| self.is_definitely_collection_expr(arm_expr, state))
                    && self.is_definitely_collection_expr(default, state)
            }
            Expr::Block { expr, .. } => self.is_definitely_collection_expr(expr, state),
            _ => false,
        }
    }
}
