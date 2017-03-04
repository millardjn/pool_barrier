use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::ptr;

/// A barrier for synchronising a main thread with the completion of work which has been offloaded to a thread pool.
/// This barrier allows blocking on `wait()` until `n` `Checkpoints` have been cleared using `check_in()` or `drop()`.
/// Threads which call check_in() do not block, in contrast to `std::sync::Barrier`, which blocks all threads and potentially deadlocks when used with an over-utilised threadpool.
/// To use and reuse the `Barrier` an `ActiveBarrier` must be generated using `activate()`, which can then be used to generate checkpoints using 'checkpoint()'.
/// An ActiveBarrier cannot be dropped without blocking until all checkpoints are cleared.
/// Generating more than `n` `Checkpoints` results in a panic. Generating less than `n` `Checkpoints` will result in an error being returned from `wait()`.
/// If a Checkpoint is passed by a panicking thread, `wait()` will return an error.
///
/// # Example
/// ```
/// use barrier::{Barrier, ActiveBarrier};
///
/// const THREADS: usize = 5;
///
/// let mut barrier = Barrier::new(THREADS);
/// run(barrier.activate());
///
/// fn run(mut barrier: ActiveBarrier){
/// 	for i in 0..THREADS{
/// 		let mut checkpoint = barrier.checkpoint();
/// 		std::thread::spawn(move||{
/// 			println!("thread_id: {}", i);           // all of these occur in arbitrary order
/// 			checkpoint.check_in();                  // this does not block the spawned thread
/// 		});
/// 	}
/// 	barrier.wait().unwrap();                        // main thread blocks here until checkpoints are cleared
/// 	println!("main thread");                        // this occurs last 
/// }
///
/// ```
pub struct Barrier{
	n: usize,
	cvar: Condvar,
	finished: Mutex<bool>,
	checkpoints_created: usize,
	checkpoints_remaining: AtomicUsize,
	checkpoint_panicked: AtomicBool,
}

impl Barrier{
	/// Create a new barrier
	/// - 'n' : the exact number of checkpoints to be generated, all of which must be cleared before `wait()` unblocks
	pub fn new(n: usize) -> Barrier{
		Barrier{
			n: n,
			cvar: Condvar::new(),
			finished: Mutex::new(false),
			checkpoints_created: 0,
			checkpoints_remaining: AtomicUsize::new(n),
			checkpoint_panicked: AtomicBool::new(false),
		}
	}
	
	/// Change the number of checkpoints that have to be cleared on the next barrier activation.
	pub fn set_n(&mut self, n: usize){
		self.n = n;
	}

	/// Activate the barrier producing an ActiveBarrier. The returned ActiveBarrier can then produce checkpoints which may be passed to worker threads, and will block on wait() or drop() until checkpoints are cleared.
	pub fn activate<'a>(&'a mut self) -> ActiveBarrier<'a>{
		self.reset();
		ActiveBarrier{barrier: self}
	}
	
	pub fn n(&self) -> usize{
		self.n
	}

	fn reset(&mut self){
		*self.finished.lock().unwrap() = false;
		self.checkpoints_created = 0;
		self.checkpoints_remaining.store(self.n, Ordering::Release);
		self.checkpoint_panicked.store(false, Ordering::Release);
	}

	fn check_in_x(&self, x: usize){
		
		let result = self.checkpoints_remaining.fetch_sub(x, Ordering::AcqRel);
		debug_assert!(result >= x); // assert that fetch_sub didnt just underflow
		debug_assert!(result <= self.n); // assert that underflow hasn't already occured
		if result == x {
			let mut finished = self.finished.lock().unwrap();
			*finished = true;
			self.cvar.notify_all();
			// Cannot use &self after this point as mutex guard drops and barrier might be dropped.
		}
	}
}

/// An ActiveBarrier can be used to generate checkpoints which must be cleared (usually by worker threads) before `wait()` and `drop()` unblock.
pub struct ActiveBarrier<'a>{
	barrier: &'a mut Barrier,
}

impl<'a> ActiveBarrier<'a>{

	/// Generate a new `Checkpoint` to be cleared.
	///
	/// # Panics
	/// This function will panics if called more than `n` times.
	pub fn checkpoint(&mut self) -> Checkpoint{
		if self.barrier.checkpoints_created >= self.barrier.n{
			panic!("More than n checkpoints generated.");
		} else {
			self.barrier.checkpoints_created +=1 ;
			Checkpoint{barrier: self.barrier as *const Barrier}
		}
	}
	
	/// Returns true if all checkpoints have been cleared and any calls to `wait()` or `drop` will not block.
	pub fn finished(&self) -> bool {
		*self.barrier.finished.lock().unwrap()
	}

	/// Block thread until all checkpoints are cleared.
	/// Returns a CheckpointPanic Err if a checkpoint is passed by a panicking thread.
	/// Returns an InsufficientCheckpoints Err if less than `n` `Checkpoint`s were generated.
	pub fn wait(&self) -> WaitResult{
		
		// Guard against deadlock if not enough checkpoints were created by falsely checking in n checkpoints.
		// This should only occur on the first call to wait(), as on subsequent calls checkpoints_remaining should be zero.
		let missing = self.barrier.n - self.barrier.checkpoints_created;
		if self.barrier.checkpoints_remaining.load(Ordering::Acquire) != 0 && missing != 0{
			self.barrier.check_in_x(missing);
		}

		// wait until all checkpoints have been passed.
		let mut finished = self.barrier.finished.lock().unwrap();
		while !*finished {
			finished = self.barrier.cvar.wait(finished).unwrap();
		}
		debug_assert_eq!(0, self.barrier.checkpoints_remaining.load(Ordering::Acquire));

		if self.barrier.checkpoint_panicked.load(Ordering::Acquire) {
			Err(WaitError::CheckpointPanic)
		} else if missing != 0 {
			Err(WaitError::InsufficientCheckpoints)
		} else {
			Ok(())
		}
	}
	
	pub fn n(&self) -> usize{
		self.barrier.n
	}
}

impl<'a> Drop for ActiveBarrier<'a>{
	fn drop(&mut self){
		self.wait().ok(); // wait for checkpoints to avoid segfault, but discard result.
	}
}

#[derive(Debug, PartialEq)]
pub enum WaitError {
	CheckpointPanic,
	InsufficientCheckpoints,
}

pub type WaitResult = Result<(), WaitError>;

/// A checkpoint which must be cleared, by calling `check_in()`, before `wait()` on the parent ActiveBarrier no longer blocks.
/// Can be sent to other threads. Automatically clears when dropped.
pub struct Checkpoint{
	barrier: *const Barrier,
}

unsafe impl Send for Checkpoint{}

impl Checkpoint{

	/// clears the checkpoint. Calling multiple times does nothing.
	pub fn check_in(&mut self){
		if !self.barrier.is_null() {
			let barrier = unsafe{&*self.barrier};
			if std::thread::panicking() {
				barrier.checkpoint_panicked.store(true, Ordering::Release);
			}
			barrier.check_in_x(1);
			self.barrier = ptr::null();
		}
	}
}

impl Drop for Checkpoint{
	fn drop(&mut self){
		self.check_in();
	}
}




/// Run tests with `cargo test -- --nocapture` to see that main thread unblocks after worker threads finish
#[cfg(test)]
mod tests{
	extern crate rand;
	use super::*;
	use tests::rand::Rng;
	const THREADS: usize = 5;

	fn threaded_run(mut barrier: ActiveBarrier){
		for i in 0..THREADS{
			let mut checkpoint = barrier.checkpoint();
			std::thread::spawn(move||{
				std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
				println!("thread_id: {}", i);         // all of these occur in arbitrary order
				checkpoint.check_in();                // this does not block the spawned thread
			});      
		}
		std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
		barrier.wait().unwrap();                      // main thread blocks here until checkpoints are cleared
		println!("main thread");                      // this occurs last 
	}

	fn panic_run(mut barrier: ActiveBarrier){
		for i in 0..THREADS{
			let mut checkpoint = barrier.checkpoint();
			std::thread::spawn(move||{
				std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
				if i%2 == 0 {panic!("Deliberate panic")};
				println!("thread_id: {}", i);
				checkpoint.check_in();
			});      
		}
		std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
		let result = barrier.wait();
		assert_eq!(result, Err(WaitError::CheckpointPanic)); // detect panic on worker thread with error
		println!("main thread");
	}

	#[test]
	fn same_thread() {
		
		fn run(mut barrier: ActiveBarrier){
			for i in 0..THREADS{
				let mut checkpoint = barrier.checkpoint();
				println!("thread_id: {}", i);
				checkpoint.check_in();
			}
			barrier.wait().unwrap();
			println!("main thread");
		}

		let mut barrier = Barrier::new(THREADS);
		run(barrier.activate());
	}

	#[test]
	fn single_use() {
		let mut barrier = Barrier::new(THREADS);
		threaded_run(barrier.activate());
	}

	#[test]
	fn reuse() {
		let mut barrier = Barrier::new(THREADS);
		threaded_run(barrier.activate());
		threaded_run(barrier.activate());
		threaded_run(barrier.activate());
		threaded_run(barrier.activate());
		threaded_run(barrier.activate());
	}

	#[test]
	fn test_checkpoint_panic_detection() {
		let mut barrier = Barrier::new(THREADS);
		panic_run(barrier.activate());
	}

	#[test]
	fn not_enough_checkpoints() {

		fn run(mut barrier: ActiveBarrier){
			for i in 0..THREADS-1{
				let mut checkpoint = barrier.checkpoint();
				std::thread::spawn(move||{
					std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
					println!("thread_id: {}", i);
					checkpoint.check_in();
				});      
			}
			std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
			let result = barrier.wait();
			assert_eq!(result, Err(WaitError::InsufficientCheckpoints)); // avoid deadlock but return error
			println!("main thread");
		}

		let mut barrier = Barrier::new(THREADS);
		run(barrier.activate());
	}

	#[test]
	#[should_panic]
	fn too_many_checkpoints() {
		fn run(mut barrier: ActiveBarrier){
			for i in 0..THREADS+1{
				let mut checkpoint = barrier.checkpoint(); // panic here to avoid creating > n checkpoints
				std::thread::spawn(move||{
					std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
					println!("thread_id: {}", i);
					checkpoint.check_in();
				});      
			}
			std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
			barrier.wait().unwrap();
			println!("main thread");
		}

		let mut barrier = Barrier::new(THREADS);
		run(barrier.activate());
	}

	#[test]
	fn test_finished() {
		fn run(mut barrier: ActiveBarrier){
			assert_eq!(false, barrier.finished());
			for i in 0..THREADS{
				let mut checkpoint = barrier.checkpoint();
				std::thread::spawn(move||{
					std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
					println!("thread_id: {}", i);
					checkpoint.check_in();
				});      
			}
			std::thread::sleep(std::time::Duration::new(0,rand::thread_rng().gen_range(1,10)*10_000_000));
			barrier.wait().unwrap();
			assert_eq!(true, barrier.finished());
			println!("main thread");
		}

		let mut barrier = Barrier::new(THREADS);
		run(barrier.activate());
	}
}


