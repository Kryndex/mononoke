// Copyright (c) 2017-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.


use futures::Async;
use futures::Poll;
use futures::stream::Stream;
use mercurial_types::{NodeHash, Repo};
use repoinfo::{Generation, RepoGenCache};
use std::boxed::Box;
use std::collections::HashMap;
use std::collections::hash_map::IntoIter;
use std::iter::IntoIterator;
use std::mem::replace;
use std::sync::Arc;

use NodeStream;
use errors::*;
use setcommon::*;

pub struct IntersectNodeStream {
    inputs: Vec<(InputStream, Poll<Option<(NodeHash, Generation)>, Error>)>,
    current_generation: Option<Generation>,
    accumulator: HashMap<NodeHash, usize>,
    drain: Option<IntoIter<NodeHash, usize>>,
}

impl IntersectNodeStream {
    pub fn new<I, R>(repo: &Arc<R>, repo_generation: RepoGenCache<R>, inputs: I) -> Self
    where
        I: IntoIterator<Item = Box<NodeStream>>,
        R: Repo,
    {
        let hash_and_gen = inputs.into_iter().map({
            move |i| {
                (
                    add_generations(i, repo_generation.clone(), repo.clone()),
                    Ok(Async::NotReady),
                )
            }
        });
        IntersectNodeStream {
            inputs: hash_and_gen.collect(),
            current_generation: None,
            accumulator: HashMap::new(),
            drain: None,
        }
    }

    fn update_current_generation(&mut self) {
        if all_inputs_ready(&self.inputs) {
            self.current_generation = self.inputs
                .iter()
                .filter_map(|&(_, ref state)| match state {
                    &Ok(Async::Ready(Some((_, gen_id)))) => Some(gen_id),
                    &Ok(Async::NotReady) => panic!("All states ready, yet some not ready!"),
                    _ => None,
                })
                .min();
        }
    }

    fn accumulate_nodes(&mut self) {
        let mut found_hashes = false;
        for &mut (_, ref mut state) in self.inputs.iter_mut() {
            if let Ok(Async::Ready(Some((hash, gen_id)))) = *state {
                if Some(gen_id) == self.current_generation {
                    *self.accumulator.entry(hash).or_insert(0) += 1;
                }
                // Inputs of higher generation than the current one get consumed and dropped
                if Some(gen_id) >= self.current_generation {
                    found_hashes = true;
                    *state = Ok(Async::NotReady);
                }
            }
        }
        if !found_hashes {
            self.current_generation = None;
        }
    }

    fn any_input_finished(&self) -> bool {
        if self.inputs.is_empty() {
            true
        } else {
            self.inputs
                .iter()
                .map(|&(_, ref state)| match state {
                    &Ok(Async::Ready(None)) => true,
                    _ => false,
                })
                .any(|done| done)
        }
    }
}

impl Stream for IntersectNodeStream {
    type Item = NodeHash;
    type Error = Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // This feels wrong, but in practice it's fine - it should be quick to hit a return, and
        // the standard futures::executor expects you to only return NotReady if blocked on I/O.
        loop {
            // Start by trying to turn as many NotReady as possible into real items
            poll_all_inputs(&mut self.inputs);

            // Empty the drain if any - return all items for this generation
            while self.drain.is_some() {
                let next_in_drain = self.drain.as_mut().and_then(|drain| drain.next());
                if next_in_drain.is_some() {
                    let (hash, count) = next_in_drain.expect("is_some() said this was safe");
                    if count == self.inputs.len() {
                        return Ok(Async::Ready(Some(hash)));
                    }
                } else {
                    self.drain = None;
                }
            }

            // Return any errors
            {
                if self.inputs.iter().any(|&(_, ref state)| state.is_err()) {
                    let inputs = replace(&mut self.inputs, Vec::new());
                    let (_, err) = inputs
                        .into_iter()
                        .find(|&(_, ref state)| state.is_err())
                        .unwrap();
                    return Err(err.unwrap_err());
                }
            }

            // If any input is not ready (we polled above), wait for them all to be ready
            if !all_inputs_ready(&self.inputs) {
                return Ok(Async::NotReady);
            }

            match self.current_generation {
                None => if self.accumulator.is_empty() {
                    self.update_current_generation();
                } else {
                    let full_accumulator = replace(&mut self.accumulator, HashMap::new());
                    self.drain = Some(full_accumulator.into_iter());
                },
                Some(_) => self.accumulate_nodes(),
            }
            // If we cannot ever output another node, we're done.
            if self.drain.is_none() && self.accumulator.is_empty() && self.any_input_finished() {
                return Ok(Async::Ready(None));
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use {NodeStream, SingleNodeHash, UnionNodeStream};
    use futures::executor::spawn;
    use linear;
    use repoinfo::RepoGenCache;
    use setcommon::NotReadyEmptyStream;
    use std::sync::Arc;
    use tests::assert_node_sequence;
    use tests::string_to_nodehash;
    use unshared_merge_even;
    use unshared_merge_uneven;

    #[test]
    fn intersect_identical_node() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let head_hash = string_to_nodehash("a5ffa77602a066db7d5cfb9fb5823a0895717c5a");
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(head_hash.clone(), &repo)),
            Box::new(SingleNodeHash::new(head_hash.clone(), &repo)),
        ];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        assert_node_sequence(repo_generation, &repo, vec![head_hash.clone()], nodestream);
    }

    #[test]
    fn intersect_three_different_nodes() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        // Note that these are *not* in generation order deliberately.
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
                &repo,
            )),
        ];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        assert_node_sequence(repo_generation, &repo, vec![], nodestream);
    }

    #[test]
    fn intersect_three_identical_nodes() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
                &repo,
            )),
        ];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
            ],
            nodestream,
        );
    }

    #[test]
    fn intersect_nesting() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
                &repo,
            )),
        ];

        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        let inputs: Vec<Box<NodeStream>> = vec![
            nodestream,
            Box::new(SingleNodeHash::new(
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
                &repo,
            )),
        ];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
            ],
            nodestream,
        );
    }

    #[test]
    fn intersection_of_unions() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
                &repo,
            )),
        ];

        let nodestream = Box::new(UnionNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        // This set has a different node sequence, so that we can demonstrate that we skip nodes
        // when they're not going to contribute.
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
                &repo,
            )),
        ];

        let nodestream2 = Box::new(UnionNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        let inputs: Vec<Box<NodeStream>> = vec![nodestream, nodestream2];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("3c15267ebf11807f3d772eb891272b911ec68759"),
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
            ],
            nodestream,
        );
    }

    #[test]
    fn intersect_error_node() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let nodehash = string_to_nodehash("0000000000000000000000000000000000000000");
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(nodehash.clone(), &repo)),
            Box::new(SingleNodeHash::new(nodehash.clone(), &repo)),
        ];
        let mut nodestream = spawn(Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation,
            inputs.into_iter(),
        )));

        assert!(
            if let Some(Err(Error(ErrorKind::NoSuchNode(hash), _))) = nodestream.wait_stream() {
                hash == nodehash
            } else {
                false
            },
            "No error for bad node"
        );
    }

    #[test]
    fn intersect_nothing() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let inputs: Vec<Box<NodeStream>> = vec![];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));
        assert_node_sequence(repo_generation, &repo, vec![], nodestream);
    }

    #[test]
    fn slow_ready_intersect_nothing() {
        // Tests that we handle an input staying at NotReady for a while without panicing
        let repeats = 10;
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(NotReadyEmptyStream {
                poll_count: repeats,
            }),
        ];
        let mut nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation,
            inputs.into_iter(),
        ));

        // Keep polling until we should be done.
        for _ in 0..repeats + 1 {
            match nodestream.poll() {
                Ok(Async::Ready(None)) => return,
                Ok(Async::NotReady) => (),
                x => panic!("Unexpected poll result {:?}", x),
            }
        }
        panic!(
            "Intersect of something that's not ready {} times failed to complete",
            repeats
        );
    }

    #[test]
    fn intersect_unshared_merge_even() {
        let repo = Arc::new(unshared_merge_even::getrepo());
        let repo_generation = RepoGenCache::new(10);

        // Post-merge, merge, and both unshared branches
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("cc7f14bc631bca43eaa32c25b04a638d54d10b70"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("d592490c4386cdb3373dd93af04d563de199b2fb"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("33fb49d8a47b29290f5163e30b294339c89505a2"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("03b0589d9788870817d03ce7b87516648ed5b33a"),
                &repo,
            )),
        ];
        let left_nodestream = Box::new(UnionNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        // Four commits from one branch
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("03b0589d9788870817d03ce7b87516648ed5b33a"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("2fa8b4ee6803a18db4649a3843a723ef1dfe852b"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("0b94a2881dda90f0d64db5fae3ee5695a38e7c8f"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("f61fdc0ddafd63503dcd8eed8994ec685bfc8941"),
                &repo,
            )),
        ];
        let right_nodestream = Box::new(UnionNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        let inputs: Vec<Box<NodeStream>> = vec![left_nodestream, right_nodestream];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("03b0589d9788870817d03ce7b87516648ed5b33a"),
            ],
            nodestream,
        );
    }

    #[test]
    fn intersect_unshared_merge_uneven() {
        let repo = Arc::new(unshared_merge_uneven::getrepo());
        let repo_generation = RepoGenCache::new(10);

        // Post-merge, merge, and both unshared branches
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("ec27ab4e7aeb7088e8a0234f712af44fb7b43a46"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("9c6dd4e2c2f43c89613b094efb426cc42afdee2a"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("64011f64aaf9c2ad2e674f57c033987da4016f51"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("03b0589d9788870817d03ce7b87516648ed5b33a"),
                &repo,
            )),
        ];
        let left_nodestream = Box::new(UnionNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        // Four commits from one branch
        let inputs: Vec<Box<NodeStream>> = vec![
            Box::new(SingleNodeHash::new(
                string_to_nodehash("03b0589d9788870817d03ce7b87516648ed5b33a"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("2fa8b4ee6803a18db4649a3843a723ef1dfe852b"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("0b94a2881dda90f0d64db5fae3ee5695a38e7c8f"),
                &repo,
            )),
            Box::new(SingleNodeHash::new(
                string_to_nodehash("f61fdc0ddafd63503dcd8eed8994ec685bfc8941"),
                &repo,
            )),
        ];
        let right_nodestream = Box::new(UnionNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        let inputs: Vec<Box<NodeStream>> = vec![left_nodestream, right_nodestream];
        let nodestream = Box::new(IntersectNodeStream::new(
            &repo,
            repo_generation.clone(),
            inputs.into_iter(),
        ));

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("03b0589d9788870817d03ce7b87516648ed5b33a"),
            ],
            nodestream,
        );
    }
}
