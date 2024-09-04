//  Copyright 2024. The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::sync::atomic::{AtomicU64, Ordering};

use tari_utilities::epoch_time::EpochTime;

/// Stats store stores statistics about running miner in memory.
pub struct StatsStore {
    start_time: AtomicU64,
    hashed_count: AtomicU64,
}

impl StatsStore {
    pub fn new() -> Self {
        Self {
            start_time: AtomicU64::new(0),
            hashed_count: AtomicU64::new(0),
        }
    }

    pub fn start(&self) {
        if self.start_time.load(Ordering::SeqCst) == 0 {
            self.start_time.swap(EpochTime::now().as_u64(), Ordering::SeqCst);
        }
    }

    pub fn hashes_per_second(&self) -> u64 {
        let elapsed = EpochTime::now().as_u64() - self.start_time.load(Ordering::SeqCst);
        self.hashed_count.load(Ordering::SeqCst) / elapsed
    }

    pub fn inc_hashed_count(&self) {
        self.hashed_count.fetch_add(1, Ordering::SeqCst);
    }
}
