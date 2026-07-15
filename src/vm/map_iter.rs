use self_cell::self_cell;

use crate::bytecode::{SharedMap, Value, VmMapIter};

self_cell!(
    struct OwnedMapIter {
        owner: SharedMap,

        #[covariant]
        dependent: VmMapIter,
    }
);

pub(crate) struct MapIteratorState {
    iter: OwnedMapIter,
    current_key: Option<Value>,
    current_value: Option<Value>,
}

impl MapIteratorState {
    pub(crate) fn new(map: SharedMap) -> Self {
        Self {
            iter: OwnedMapIter::new(map, |map| map.iter()),
            current_key: None,
            current_value: None,
        }
    }

    pub(crate) fn advance(&mut self) -> bool {
        let next = self.iter.with_dependent_mut(|_, iter| {
            iter.next().map(|(key, value)| (key.clone(), value.clone()))
        });
        match next {
            Some((key, value)) => {
                self.current_key = Some(key);
                self.current_value = Some(value);
                true
            }
            None => {
                self.current_key = None;
                self.current_value = None;
                false
            }
        }
    }

    pub(crate) fn take_key(&mut self) -> Option<Value> {
        self.current_key.take()
    }

    pub(crate) fn take_value(&mut self) -> Option<Value> {
        self.current_value.take()
    }
}
