// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use crate::serialize::error::SerializeError;
use crate::serialize::per_type::dict::ZeroDictSerializer;
use crate::serialize::serializer::PyObjectSerializer;
use crate::serialize::state::SerializerState;
use crate::str::PyStr;
use crate::typeref::{
    DATACLASS_FIELDS_STR, DICT_STR, FIELD_TYPE, FIELD_TYPE_STR, SLOTS_STR, STR_TYPE,
};
use crate::util::isize_to_usize;

use serde::ser::{Serialize, SerializeMap, Serializer};

use core::ptr::NonNull;

#[repr(transparent)]
pub(crate) struct DataclassGenericSerializer<'a> {
    previous: &'a PyObjectSerializer,
}

impl<'a> DataclassGenericSerializer<'a> {
    pub fn new(previous: &'a PyObjectSerializer) -> Self {
        Self { previous: previous }
    }
}

impl Serialize for DataclassGenericSerializer<'_> {
    #[inline(never)]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if unlikely!(self.previous.state.recursion_limit()) {
            err!(SerializeError::RecursionLimit)
        }
        let dict = ffi!(PyObject_GetAttr(self.previous.ptr, DICT_STR));
        let ob_type = ob_type!(self.previous.ptr);
        if unlikely!(dict.is_null()) {
            ffi!(PyErr_Clear());
            DataclassFallbackSerializer::new(
                self.previous.ptr,
                self.previous.state,
                self.previous.default,
            )
            .serialize(serializer)
        } else if pydict_contains!(ob_type, SLOTS_STR) {
            let ret = DataclassFallbackSerializer::new(
                self.previous.ptr,
                self.previous.state,
                self.previous.default,
            )
            .serialize(serializer);
            ffi!(Py_DECREF(dict));
            ret
        } else {
            let ret =
                DataclassFastSerializer::new(dict, self.previous.state, self.previous.default)
                    .serialize(serializer);
            ffi!(Py_DECREF(dict));
            ret
        }
    }
}

pub(crate) struct DataclassFastSerializer {
    ptr: *mut pyo3_ffi::PyObject,
    state: SerializerState,
    default: Option<NonNull<pyo3_ffi::PyObject>>,
}

impl DataclassFastSerializer {
    pub fn new(
        ptr: *mut pyo3_ffi::PyObject,
        state: SerializerState,
        default: Option<NonNull<pyo3_ffi::PyObject>>,
    ) -> Self {
        DataclassFastSerializer {
            ptr: ptr,
            state: state.copy_for_recursive_call(),
            default: default,
        }
    }
}

impl Serialize for DataclassFastSerializer {
    #[inline(never)]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let len = isize_to_usize(ffi!(Py_SIZE(self.ptr)));
        if unlikely!(len == 0) {
            return ZeroDictSerializer::new().serialize(serializer);
        }
        let mut map = serializer.serialize_map(None).unwrap();

        let mut pos = 0;
        let mut next_key: *mut pyo3_ffi::PyObject = core::ptr::null_mut();
        let mut next_value: *mut pyo3_ffi::PyObject = core::ptr::null_mut();

        pydict_next!(self.ptr, &mut pos, &mut next_key, &mut next_value);

        for _ in 0..len {
            let key = next_key;
            let value = next_value;

            pydict_next!(self.ptr, &mut pos, &mut next_key, &mut next_value);

            let key_as_str = {
                let key_ob_type = ob_type!(key);
                if unlikely!(!is_class_by_type!(key_ob_type, STR_TYPE)) {
                    err!(SerializeError::KeyMustBeStr)
                }
                match unsafe { PyStr::from_ptr_unchecked(key).to_str() } {
                    Some(uni) => uni,
                    None => err!(SerializeError::InvalidStr),
                }
            };
            if unlikely!(key_as_str.as_bytes()[0] == b'_') {
                continue;
            }
            let pyvalue = PyObjectSerializer::new(value, self.state, self.default);
            map.serialize_key(key_as_str).unwrap();
            map.serialize_value(&pyvalue)?;
        }
        map.end()
    }
}

pub(crate) struct DataclassFallbackSerializer {
    ptr: *mut pyo3_ffi::PyObject,
    state: SerializerState,
    default: Option<NonNull<pyo3_ffi::PyObject>>,
}

impl DataclassFallbackSerializer {
    pub fn new(
        ptr: *mut pyo3_ffi::PyObject,
        state: SerializerState,
        default: Option<NonNull<pyo3_ffi::PyObject>>,
    ) -> Self {
        DataclassFallbackSerializer {
            ptr: ptr,
            state: state.copy_for_recursive_call(),
            default: default,
        }
    }
}

impl Serialize for DataclassFallbackSerializer {
    #[cold]
    #[inline(never)]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let fields = ffi!(PyObject_GetAttr(self.ptr, DATACLASS_FIELDS_STR));
        debug_assert!(ffi!(Py_REFCNT(fields)) >= 2);
        ffi!(Py_DECREF(fields));
        let len = isize_to_usize(ffi!(Py_SIZE(fields)));
        if unlikely!(len == 0) {
            return ZeroDictSerializer::new().serialize(serializer);
        }
        let mut map = serializer.serialize_map(None).unwrap();

        let mut pos = 0;
        let mut next_key: *mut pyo3_ffi::PyObject = core::ptr::null_mut();
        let mut next_value: *mut pyo3_ffi::PyObject = core::ptr::null_mut();

        pydict_next!(fields, &mut pos, &mut next_key, &mut next_value);

        for _ in 0..len {
            let attr = next_key;
            let field = next_value;

            pydict_next!(fields, &mut pos, &mut next_key, &mut next_value);

            let field_type = ffi!(PyObject_GetAttr(field, FIELD_TYPE_STR));
            debug_assert!(ffi!(Py_REFCNT(field_type)) >= 2);
            ffi!(Py_DECREF(field_type));
            if unsafe { !core::ptr::eq(field_type.cast::<pyo3_ffi::PyTypeObject>(), FIELD_TYPE) } {
                continue;
            }

            let key_as_str = match unsafe { PyStr::from_ptr_unchecked(attr).to_str() } {
                Some(uni) => uni,
                None => err!(SerializeError::InvalidStr),
            };
            if key_as_str.as_bytes()[0] == b'_' {
                continue;
            }

            let value = ffi!(PyObject_GetAttr(self.ptr, attr));
            debug_assert!(ffi!(Py_REFCNT(value)) >= 2);
            ffi!(Py_DECREF(value));
            let pyvalue = PyObjectSerializer::new(value, self.state, self.default);

            map.serialize_key(key_as_str).unwrap();
            map.serialize_value(&pyvalue)?;
        }
        map.end()
    }
}
