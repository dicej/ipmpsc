use std::{marker::PhantomData, mem};

#[derive(Copy, Clone, Default)]
pub struct BitMask(u128);

impl BitMask {
    pub const fn capacity() -> u8 {
        (mem::size_of::<Self>() * 8) as u8
    }

    pub fn ones(self) -> impl Iterator<Item = u8> {
        struct Ones;

        impl Strategy for Ones {
            fn accept(value: bool) -> bool {
                value
            }
        }

        BitMaskIterator::<Ones> {
            mask: self,
            index: 0,
            _data: PhantomData,
        }
    }

    pub fn zeros(self) -> impl Iterator<Item = u8> {
        struct Zeros;

        impl Strategy for Zeros {
            fn accept(value: bool) -> bool {
                !value
            }
        }

        BitMaskIterator::<Zeros> {
            mask: self,
            index: 0,
            _data: PhantomData,
        }
    }

    pub fn get(self, index: u8) -> bool {
        (self.0 & 1_u128.checked_shl(index.into()).unwrap()) != 0
    }

    pub fn set(self, index: u8) -> Self {
        Self(self.0 | 1_u128.checked_shl(index.into()).unwrap())
    }

    pub fn clear(&self, index: u8) -> Self {
        Self(self.0 & !(1_u128.checked_shl(index.into()).unwrap()))
    }
}

trait Strategy {
    fn accept(value: bool) -> bool;
}

struct BitMaskIterator<T> {
    mask: BitMask,
    index: u8,
    _data: PhantomData<T>,
}

impl<T: Strategy> Iterator for BitMaskIterator<T> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        for index in self.index..BitMask::capacity() {
            if T::accept(self.mask.get(index)) {
                self.index = index + 1;
                return Some(index);
            }
        }

        None
    }
}
