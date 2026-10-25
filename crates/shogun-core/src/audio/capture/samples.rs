//! Bounded ingress shared by mic and tap. At most 32 × 320 f32 samples (40 KiB) per source.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};

pub(super) struct SampleSender {
    tx: SampleTx,
    offset: AtomicU64,
    dropped: Arc<AtomicU64>,
}
enum SampleTx {
    Bounded(mpsc::SyncSender<(Vec<f32>, Option<u64>)>),
    Unbounded(mpsc::Sender<(Vec<f32>, Option<u64>)>),
}
pub(super) struct SampleReceiver {
    rx: mpsc::Receiver<(Vec<f32>, Option<u64>)>,
    dropped: Arc<AtomicU64>,
}
pub(super) fn channel(bounded: bool) -> (SampleSender, SampleReceiver) {
    // The synchronous utterance/Whisper worker has different buffering needs; preserve its
    // existing channel. Only the meeting live lane opts into this bounded ingress.
    let (tx, rx) = if bounded {
        let (tx, rx) = mpsc::sync_channel(32);
        (SampleTx::Bounded(tx), rx)
    } else {
        let (tx, rx) = mpsc::channel();
        (SampleTx::Unbounded(tx), rx)
    };
    let dropped = Arc::new(AtomicU64::new(0));
    (
        SampleSender {
            tx,
            offset: AtomicU64::new(0),
            dropped: dropped.clone(),
        },
        SampleReceiver { rx, dropped },
    )
}
impl SampleSender {
    pub(super) fn send(&self, samples: Vec<f32>) {
        let tx = match &self.tx {
            SampleTx::Bounded(tx) => tx,
            SampleTx::Unbounded(tx) => {
                let _ = tx.send((samples, None));
                return;
            }
        };
        for chunk in samples.chunks(320) {
            let offset = self.offset.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            match tx.try_send((chunk.to_vec(), Some(offset))) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(frame)) => {
                    self.dropped
                        .fetch_add(frame.0.len() as u64, Ordering::Relaxed);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    }
}
impl SampleReceiver {
    #[cfg(test)]
    pub(super) fn try_recv(&self) -> Result<Vec<f32>, mpsc::TryRecvError> {
        self.try_recv_with_offset().map(|(samples, _)| samples)
    }
    pub(super) fn try_recv_with_offset(
        &self,
    ) -> Result<(Vec<f32>, Option<u64>), mpsc::TryRecvError> {
        self.rx.try_recv()
    }
    pub(super) fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slow_consumer_has_bounded_ingress_and_observable_loss() {
        let (tx, rx) = channel(true);
        tx.send(vec![0.0; 320 * 100]);
        assert_eq!(rx.take_dropped(), 320 * 68);
        assert_eq!(rx.take_dropped(), 0);
        for _ in 0..32 {
            assert_eq!(rx.try_recv().map(|x| x.len()), Ok(320));
        }
        assert!(rx.try_recv().is_err());
        tx.send(vec![0.5; 320]);
        let next = rx.try_recv_with_offset();
        assert!(next
            .is_ok_and(|(samples, offset)| samples == vec![0.5; 320] && offset == Some(320 * 100)));
        drop(rx);
        tx.send(vec![0.0; 320]);
    }
}
