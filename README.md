# pool_barrier
A barrier for blocking a main thread until the completion of work which has been offloaded to worker threads,
without blocking the worker threads (in contrast to `std::sync::Barrier` which blocks all threads).
Mainly useful for not deadlocking a threadpool by submitting more work items than there are threads.

[docs](https://docs.rs/pool_barrier)


```
use pool_barrier::{Barrier, ActiveBarrier};

const THREADS: usize = 5;

let mut barrier = Barrier::new(THREADS);
run(barrier.activate());
run(barrier.activate());                            // a barrier can be reused once checkpoints are cleared

fn run(mut barrier: ActiveBarrier){
	for i in 0..THREADS{
		let mut checkpoint = barrier.checkpoint();
		std::thread::spawn(move||{
			println!("thread_id: {}", i);           // all of these occur in arbitrary order
			checkpoint.check_in();                  // this does not block the spawned thread
		});
	}
	barrier.wait().unwrap();                        // main thread blocks here until all checkpoints are cleared
	println!("main thread");                        // this occurs last 
}
```
