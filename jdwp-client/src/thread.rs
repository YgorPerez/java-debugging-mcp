// ThreadReference command implementations
//
// Commands for working with threads (frames, status, suspend/resume)

use crate::commands::{command_sets, thread_commands};
use crate::connection::JdwpConnection;
use crate::protocol::{CommandPacket, JdwpResult};
use crate::reader::{read_i32, read_string, read_u64};
use crate::types::{FrameId, Location, ThreadId};
use bytes::BufMut;
use serde::{Deserialize, Serialize};

/// Stack frame information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub frame_id: FrameId,
    pub location: Location,
}

impl JdwpConnection {
    /// Get stack frames for a thread (ThreadReference.Frames command)
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_frames(
        &mut self,
        thread_id: ThreadId,
        start_frame: i32,
        length: i32,
    ) -> JdwpResult<Vec<Frame>> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::THREAD_REFERENCE, thread_commands::FRAMES);

        // Write thread ID
        packet.data.put_u64(thread_id);
        // Start frame (0 = current/top frame)
        packet.data.put_i32(start_frame);
        // Length (-1 = all frames)
        packet.data.put_i32(length);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        // Read number of frames
        let frames_count = read_i32(&mut data)?;
        let mut frames = Vec::with_capacity(usize::try_from(frames_count).unwrap_or(0));

        for _ in 0..frames_count {
            let frame_id = read_u64(&mut data)?;

            // Read location
            let type_tag = crate::reader::read_u8(&mut data)?;
            let class_id = read_u64(&mut data)?;
            let method_id = read_u64(&mut data)?;
            let index = read_u64(&mut data)?;

            frames.push(Frame {
                frame_id,
                location: Location {
                    type_tag,
                    class_id,
                    method_id,
                    index,
                },
            });
        }

        Ok(frames)
    }

    /// Get all threads (VirtualMachine.AllThreads)
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_all_threads(&mut self) -> JdwpResult<Vec<ThreadId>> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, crate::commands::vm_commands::ALL_THREADS);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();

        let threads_count = read_i32(&mut data)?;
        let mut threads = Vec::with_capacity(usize::try_from(threads_count).unwrap_or(0));

        for _ in 0..threads_count {
            threads.push(read_u64(&mut data)?);
        }

        Ok(threads)
    }

    /// Get a thread's name (ThreadReference.Name).
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_thread_name(&mut self, thread_id: ThreadId) -> JdwpResult<String> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::THREAD_REFERENCE, thread_commands::NAME);
        packet.data.put_u64(thread_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        read_string(&mut data)
    }

    /// Get a thread's (`thread_status`, `suspend_status`) (ThreadReference.Status).
    /// `suspend_status` != 0 means the thread is currently suspended.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn get_thread_status(&mut self, thread_id: ThreadId) -> JdwpResult<(i32, i32)> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::THREAD_REFERENCE, thread_commands::STATUS);
        packet.data.put_u64(thread_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        let thread_status = read_i32(&mut data)?;
        let suspend_status = read_i32(&mut data)?;
        Ok((thread_status, suspend_status))
    }

    /// How many times this thread has been suspended (`ThreadReference.SuspendCount`).
    ///
    /// JDWP **counts** suspends: a thread suspended n times must be resumed n times before it runs
    /// again. That makes this the only way to answer "did my resume actually resume it?" — a single
    /// `resume_all` against a count of 2 leaves the thread stopped while every command still succeeds.
    /// Verified against a real JVM: two `Suspend`s then one `Resume` leaves the debuggee stopped.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn suspend_count(&mut self, thread_id: ThreadId) -> JdwpResult<i32> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(
            id,
            command_sets::THREAD_REFERENCE,
            crate::commands::thread_commands::SUSPEND_COUNT,
        );
        packet.data.put_u64(thread_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        let mut data = reply.data();
        read_i32(&mut data)
    }

    /// Suspend all threads (VirtualMachine.Suspend)
    ///
    /// Suspends are **counted** — calling this twice needs two resumes. Callers that mean "make sure it
    /// is stopped" should check [`suspend_count`](Self::suspend_count) first rather than suspending
    /// again, or they will build a depth that a single resume can't undo.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn suspend_all(&mut self) -> JdwpResult<()> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, crate::commands::vm_commands::SUSPEND);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        Ok(())
    }

    /// Resume all threads (VirtualMachine.Resume) — **one** decrement of every thread's suspend count.
    ///
    /// Not the same as "make the VM run": if anything suspended it twice, this leaves it stopped and
    /// still reports success. Use [`resume_all_fully`](Self::resume_all_fully) when the intent is that
    /// the application actually continues.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn resume_all(&mut self) -> JdwpResult<()> {
        let id = self.next_id();
        let packet = CommandPacket::new(id, command_sets::VIRTUAL_MACHINE, crate::commands::vm_commands::RESUME);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        Ok(())
    }

    /// Resume until the application is actually running, not just once.
    ///
    /// Returns `(resumes issued, remaining suspend count)` — a remaining count of 0 means the VM is
    /// genuinely going again. `probe_thread` is the thread whose count is checked; any live thread works
    /// for a VM-wide suspend, since `VirtualMachine.Suspend` increments all of them.
    ///
    /// This exists because "resume" and "is it running" are different questions in JDWP, and a caller
    /// whose job is to un-freeze a shared JVM (a watchdog, a panic button) must not report success on
    /// the strength of a command that returned OK while the debuggee stayed stopped.
    ///
    /// Bounded by `max_resumes` so a pathological count can't spin forever; a thread that is *also*
    /// suspended individually (an `EventThread`-policy event) may legitimately need more than one.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if a JDWP request fails or a reply cannot be parsed.
    pub async fn resume_all_fully(
        &mut self,
        probe_thread: ThreadId,
        max_resumes: u32,
    ) -> JdwpResult<(u32, i32)> {
        let mut issued = 0;
        for _ in 0..max_resumes {
            self.resume_all().await?;
            issued += 1;
            // A dead/invalid thread can't report a count; treat that as "nothing left to resume"
            // rather than looping, since the thread we were watching has gone.
            let left = self.suspend_count(probe_thread).await.unwrap_or(0);
            if left <= 0 {
                return Ok((issued, 0));
            }
        }
        let left = self.suspend_count(probe_thread).await.unwrap_or(0);
        Ok((issued, left))
    }

    /// Resume a single thread (ThreadReference.Resume) — decrements just that thread's suspend
    /// count, leaving other suspended threads alone. Used after arming a deferred breakpoint on the
    /// thread that a `ClassPrepare` event suspended, so class init proceeds without disturbing any
    /// thread parked at a real breakpoint.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn resume_thread(&mut self, thread_id: ThreadId) -> JdwpResult<()> {
        let id = self.next_id();
        let mut packet = CommandPacket::new(id, command_sets::THREAD_REFERENCE, thread_commands::RESUME);
        packet.data.put_u64(thread_id);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        Ok(())
    }

    /// Force the topmost frame of a suspended thread to return `value` immediately
    /// (ThreadReference.ForceEarlyReturn). The thread must be suspended and the value's tag must be
    /// assignable to the method's declared return type — pass a `Void` value for a `void` method.
    /// Lets a caller short-circuit a method (e.g. make a rejecting `salvar` return `true`) without
    /// editing and redeploying code. Requires the JVM's `canForceEarlyReturn` capability.
    ///
    /// # Errors
    /// Returns a [`JdwpError`] if the JDWP request fails or the reply cannot be parsed.
    pub async fn force_early_return(
        &mut self,
        thread_id: ThreadId,
        value: &crate::types::Value,
    ) -> JdwpResult<()> {
        self.guard_invocation("a forced early return")?;
        let id = self.next_id();
        let mut packet =
            CommandPacket::new(id, command_sets::THREAD_REFERENCE, thread_commands::FORCE_EARLY_RETURN);
        packet.data.put_u64(thread_id);
        crate::eval::write_tagged_value(&mut packet.data, value);

        let reply = self.send_command(packet).await?;
        reply.check_error()?;

        Ok(())
    }
}
