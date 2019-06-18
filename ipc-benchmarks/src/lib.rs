#![feature(test)]

#[macro_use]
extern crate serde_derive;
extern crate test;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct YuvFrameInfo {
    pub width: u32,
    pub height: u32,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
}

#[derive(Serialize, Deserialize)]
pub struct YuvFrame<'a> {
    pub info: YuvFrameInfo,
    #[serde(with = "serde_bytes")]
    pub y_pixels: &'a [u8],
    #[serde(with = "serde_bytes")]
    pub u_pixels: &'a [u8],
    #[serde(with = "serde_bytes")]
    pub v_pixels: &'a [u8],
}

#[derive(Serialize, Deserialize)]
pub struct OwnedYuvFrame {
    pub info: YuvFrameInfo,
    #[serde(with = "serde_bytes")]
    pub y_pixels: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub u_pixels: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub v_pixels: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use failure::Error;
    use ipc_channel::ipc;
    use ipmpsc::{Receiver, Sender};
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering::SeqCst},
            Arc,
        },
        thread,
    };
    use test::Bencher;

    const WIDTH: usize = 3840;
    const HEIGHT: usize = 2160;
    const Y_STRIDE: usize = WIDTH;
    const U_STRIDE: usize = WIDTH / 2;
    const V_STRIDE: usize = WIDTH / 2;

    #[bench]
    fn bench_ipmpsc(b: &mut Bencher) -> Result<(), Error> {
        let (name, mut rx) = Receiver::temp_file(32 * 1024 * 1024)?;
        let tx = Sender::from_path(&name)?;
        let alive = Arc::new(AtomicBool::new(true));

        thread::spawn({
            let alive = alive.clone();
            move || {
                let y_pixels = vec![128_u8; Y_STRIDE * HEIGHT];
                let u_pixels = vec![192_u8; U_STRIDE * HEIGHT / 2];
                let v_pixels = vec![255_u8; V_STRIDE * HEIGHT / 2];

                let frame = YuvFrame {
                    info: YuvFrameInfo {
                        width: WIDTH as u32,
                        height: HEIGHT as u32,
                        y_stride: Y_STRIDE as u32,
                        u_stride: U_STRIDE as u32,
                        v_stride: V_STRIDE as u32,
                    },
                    y_pixels: &y_pixels,
                    u_pixels: &u_pixels,
                    v_pixels: &v_pixels,
                };

                while alive.load(SeqCst) {
                    if let Err(e) = tx.send(&frame) {
                        panic!("error sending: {:?}", e);
                    }
                }
            }
        });

        // wait for first frame to arrive
        {
            let mut context = rx.zero_copy_context();
            match context.recv::<YuvFrame>() {
                Err(e) => panic!("error receiving: {:?}", e),
                Ok(_) => (),
            };
        }

        b.iter(|| {
            let mut context = rx.zero_copy_context();
            match context.recv::<YuvFrame>() {
                Err(e) => panic!("error receiving: {:?}", e),
                Ok(frame) => test::black_box(&frame),
            };
        });

        alive.store(false, SeqCst);

        loop {
            let mut context = rx.zero_copy_context();
            match context.try_recv::<YuvFrame>()? {
                Some(_) => (),
                None => break,
            }
        }

        Ok(())
    }

    #[bench]
    fn bench_ipc_channel(b: &mut Bencher) -> Result<(), Error> {
        let (tx, rx) = ipc::channel()?;
        let alive = Arc::new(AtomicBool::new(true));

        thread::spawn({
            let alive = alive.clone();
            move || {
                while alive.load(SeqCst) {
                    let y_pixels = vec![128_u8; Y_STRIDE * HEIGHT];
                    let u_pixels = vec![192_u8; U_STRIDE * HEIGHT / 2];
                    let v_pixels = vec![255_u8; V_STRIDE * HEIGHT / 2];

                    let frame = OwnedYuvFrame {
                        info: YuvFrameInfo {
                            width: WIDTH as u32,
                            height: HEIGHT as u32,
                            y_stride: Y_STRIDE as u32,
                            u_stride: U_STRIDE as u32,
                            v_stride: V_STRIDE as u32,
                        },
                        y_pixels,
                        u_pixels,
                        v_pixels,
                    };

                    if let Err(e) = tx.send(frame) {
                        if alive.load(SeqCst) {
                            panic!("error sending: {:?}", e);
                        }
                    }
                }
            }
        });

        // wait for first frame to arrive
        match rx.recv() {
            Err(e) => panic!("error receiving: {:?}", e),
            Ok(_) => (),
        }

        b.iter(|| {
            match rx.recv() {
                Err(e) => panic!("error receiving: {:?}", e),
                Ok(frame) => test::black_box(&frame),
            };
        });

        alive.store(false, SeqCst);

        Ok(())
    }

    #[bench]
    fn bench_ipc_channel_bytes(b: &mut Bencher) -> Result<(), Error> {
        let (tx, rx) = ipc::bytes_channel()?;
        let alive = Arc::new(AtomicBool::new(true));

        thread::spawn({
            let alive = alive.clone();
            move || {
                let y_pixels = vec![128_u8; Y_STRIDE * HEIGHT];
                let u_pixels = vec![192_u8; U_STRIDE * HEIGHT / 2];
                let v_pixels = vec![255_u8; V_STRIDE * HEIGHT / 2];

                let frame = YuvFrame {
                    info: YuvFrameInfo {
                        width: WIDTH as u32,
                        height: HEIGHT as u32,
                        y_stride: Y_STRIDE as u32,
                        u_stride: U_STRIDE as u32,
                        v_stride: V_STRIDE as u32,
                    },
                    y_pixels: &y_pixels,
                    u_pixels: &u_pixels,
                    v_pixels: &v_pixels,
                };

                let size = bincode::serialized_size(&frame).unwrap() as usize;
                let mut buffer = vec![0_u8; size];

                while alive.load(SeqCst) {
                    bincode::serialize_into(&mut buffer as &mut [u8], &frame).unwrap();
                    if let Err(e) = tx.send(&buffer) {
                        if alive.load(SeqCst) {
                            panic!("error sending: {:?}", e);
                        }
                    }
                }
            }
        });

        // wait for first frame to arrive
        match rx.recv() {
            Err(e) => panic!("error receiving: {:?}", e),
            Ok(_) => (),
        }

        b.iter(|| {
            match rx.recv() {
                Err(e) => panic!("error receiving: {:?}", e),
                Ok(frame) => test::black_box(bincode::deserialize::<YuvFrame>(&frame).unwrap()),
            };
        });

        alive.store(false, SeqCst);

        Ok(())
    }
}
