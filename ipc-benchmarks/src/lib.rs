#![feature(test)]

extern crate test;

use serde_derive::{Deserialize, Serialize};

use std::time::Duration;

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
    use anyhow::{anyhow, Error, Result};
    use ipc_channel::ipc;
    use ipmpsc::{Receiver, Sender, SharedRingBuffer};
    use test::Bencher;

    const SMALL: (usize, usize) = (3, 2);
    const LARGE: (usize, usize) = (3840, 2160);

    fn y_stride(width: usize) -> usize {
        width
    }

    fn uv_stride(width: usize) -> usize {
        width / 2
    }

    #[bench]
    fn bench_ipmpsc_small(bencher: &mut Bencher) -> Result<()> {
        bench_ipmpsc(bencher, SMALL)
    }

    #[bench]
    fn bench_ipmpsc_large(bencher: &mut Bencher) -> Result<()> {
        bench_ipmpsc(bencher, LARGE)
    }

    fn bench_ipmpsc(bencher: &mut Bencher, (width, height): (usize, usize)) -> Result<()> {
        let (name, buffer) = SharedRingBuffer::create_temp(32 * 1024 * 1024)?;
        let mut rx = Receiver::new(buffer);

        let (exit_name, exit_buffer) = SharedRingBuffer::create_temp(1)?;
        let exit_tx = Sender::new(exit_buffer);

        let sender = ipmpsc::fork(move || {
            let buffer = SharedRingBuffer::open(&name)?;
            let tx = Sender::new(buffer);

            let exit_buffer = SharedRingBuffer::open(&exit_name)?;
            let exit_rx = Receiver::new(exit_buffer);

            let y_pixels = vec![128_u8; y_stride(width) * height];
            let u_pixels = vec![192_u8; uv_stride(width) * height / 2];
            let v_pixels = vec![255_u8; uv_stride(width) * height / 2];

            let frame = YuvFrame {
                info: YuvFrameInfo {
                    width: width as _,
                    height: height as _,
                    y_stride: y_stride(width) as _,
                    u_stride: uv_stride(width) as _,
                    v_stride: uv_stride(width) as _,
                },
                y_pixels: &y_pixels,
                u_pixels: &u_pixels,
                v_pixels: &v_pixels,
            };

            while exit_rx.try_recv::<u8>()?.is_none() {
                tx.send_timeout(&frame, Duration::from_millis(100))?;
            }

            Ok(())
        })?;

        // wait for first frame to arrive
        {
            let mut context = rx.zero_copy_context();
            if let Err(e) = context.recv::<YuvFrame>() {
                panic!("error receiving: {:?}", e);
            };
        }

        bencher.iter(|| {
            let mut context = rx.zero_copy_context();
            match context.recv::<YuvFrame>() {
                Err(e) => panic!("error receiving: {:?}", e),
                Ok(frame) => test::black_box(&frame),
            };
        });

        exit_tx.send(&1_u8)?;

        sender.join().map_err(|e| anyhow!("{:?}", e))??;

        Ok(())
    }

    #[bench]
    fn bench_ipc_channel_small(bencher: &mut Bencher) -> Result<()> {
        bench_ipc_channel(bencher, SMALL)
    }

    #[bench]
    fn bench_ipc_channel_large(bencher: &mut Bencher) -> Result<()> {
        bench_ipc_channel(bencher, LARGE)
    }

    fn bench_ipc_channel(bencher: &mut Bencher, (width, height): (usize, usize)) -> Result<()> {
        let (tx, rx) = ipc::channel()?;

        let (exit_name, exit_buffer) = SharedRingBuffer::create_temp(1)?;
        let exit_tx = Sender::new(exit_buffer);

        let sender = ipmpsc::fork(move || {
            let exit_buffer = SharedRingBuffer::open(&exit_name)?;
            let exit_rx = Receiver::new(exit_buffer);

            while exit_rx.try_recv::<u8>()?.is_none() {
                let y_pixels = vec![128_u8; y_stride(width) * height];
                let u_pixels = vec![192_u8; uv_stride(width) * height / 2];
                let v_pixels = vec![255_u8; uv_stride(width) * height / 2];

                let frame = OwnedYuvFrame {
                    info: YuvFrameInfo {
                        width: width as _,
                        height: height as _,
                        y_stride: y_stride(width) as _,
                        u_stride: uv_stride(width) as _,
                        v_stride: uv_stride(width) as _,
                    },
                    y_pixels,
                    u_pixels,
                    v_pixels,
                };

                if let Err(e) = tx.send(frame) {
                    if exit_rx.try_recv::<u8>()?.is_none() {
                        return Err(Error::from(e));
                    } else {
                        break;
                    }
                }
            }

            Ok(())
        })?;

        // wait for first frame to arrive
        rx.recv().map_err(|e| anyhow!("{:?}", e))?;

        bencher.iter(|| {
            match rx.recv() {
                Err(e) => panic!("error receiving: {:?}", e),
                Ok(frame) => test::black_box(&frame),
            };
        });

        exit_tx.send(&1_u8)?;

        while rx.recv().is_ok() {}

        sender.join().map_err(|e| anyhow!("{:?}", e))??;

        Ok(())
    }

    #[bench]
    fn bench_ipc_channel_bytes_small(bencher: &mut Bencher) -> Result<()> {
        bench_ipc_channel_bytes(bencher, SMALL)
    }

    #[bench]
    fn bench_ipc_channel_bytes_large(bencher: &mut Bencher) -> Result<()> {
        bench_ipc_channel_bytes(bencher, LARGE)
    }

    fn bench_ipc_channel_bytes(
        bencher: &mut Bencher,
        (width, height): (usize, usize),
    ) -> Result<()> {
        let (tx, rx) = ipc::bytes_channel()?;

        let (exit_name, exit_buffer) = SharedRingBuffer::create_temp(1)?;
        let exit_tx = Sender::new(exit_buffer);

        let sender = ipmpsc::fork(move || {
            let exit_buffer = SharedRingBuffer::open(&exit_name)?;
            let exit_rx = Receiver::new(exit_buffer);

            let y_pixels = vec![128_u8; y_stride(width) * height];
            let u_pixels = vec![192_u8; uv_stride(width) * height / 2];
            let v_pixels = vec![255_u8; uv_stride(width) * height / 2];

            let frame = YuvFrame {
                info: YuvFrameInfo {
                    width: width as _,
                    height: height as _,
                    y_stride: y_stride(width) as _,
                    u_stride: uv_stride(width) as _,
                    v_stride: uv_stride(width) as _,
                },
                y_pixels: &y_pixels,
                u_pixels: &u_pixels,
                v_pixels: &v_pixels,
            };

            let size = bincode::serialized_size(&frame).unwrap() as usize;
            let mut buffer = vec![0_u8; size];

            while exit_rx.try_recv::<u8>()?.is_none() {
                bincode::serialize_into(&mut buffer as &mut [u8], &frame).unwrap();
                if let Err(e) = tx.send(&buffer) {
                    if exit_rx.try_recv::<u8>()?.is_none() {
                        return Err(Error::from(e));
                    } else {
                        break;
                    }
                }
            }

            Ok(())
        })?;

        // wait for first frame to arrive
        rx.recv().map_err(|e| anyhow!("{:?}", e))?;

        bencher.iter(|| {
            match rx.recv() {
                Err(e) => panic!("error receiving: {:?}", e),
                Ok(frame) => test::black_box(bincode::deserialize::<YuvFrame>(&frame).unwrap()),
            };
        });

        exit_tx.send(&1_u8)?;

        while rx.recv().is_ok() {}

        sender.join().map_err(|e| anyhow!("{:?}", e))??;

        Ok(())
    }
}
