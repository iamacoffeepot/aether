//! Which files each Muse session looked at, folded from the journal.

use std::collections::{BTreeMap, BTreeSet};

use aether_bloomery_kinds::{RequestSource, Requested, Seq, Transition};
use aether_bloomery_program::{At, Cited, Program, Reactor, ViewCursor, view};
use aether_data::{Digest, Ref};

use super::{ContinueInput, MuseSession, SessionContinue, SessionKey, SessionOpen};
use crate::tools::{TreeGrep, TreeList, TreeRead};

/// The file-access record of one session: the `tree.read`, `tree.list`, and
/// `tree.grep` inputs it ran, by input digest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionReads {
    read: BTreeSet<Digest>,
    listed: BTreeSet<Digest>,
    searched: BTreeSet<Digest>,
}

impl SessionReads {
    /// The inputs of the session's `tree.read` runs.
    #[must_use]
    pub fn read_inputs(&self) -> &BTreeSet<Digest> {
        &self.read
    }

    /// The inputs of the session's `tree.list` runs.
    #[must_use]
    pub fn list_inputs(&self) -> &BTreeSet<Digest> {
        &self.listed
    }

    /// The inputs of the session's `tree.grep` runs.
    #[must_use]
    pub fn grep_inputs(&self) -> &BTreeSet<Digest> {
        &self.searched
    }
}

/// Which files each Muse session read or searched, folded from the journal.
///
/// `Reads` records the `tree.read`, `tree.list`, and `tree.grep` inputs of
/// every session by [`SessionKey`], so a reviewer can tell what a session
/// looked at without replaying the whole journal. Each set holds the input
/// digest of the matching runs; use [`SessionReads`] to read them.
///
/// To resolve a recorded digest to a path, with artifact access, fetch the
/// input artifact and decode [`aether_bloomery_program::ErasedTooled`], take
/// its `args` ([`aether_data::ErasedRef`]), check the kind against the args
/// kind for the matching category ([`crate::tools::ReadArgs`],
/// [`crate::tools::ListArgs`], or [`crate::tools::GrepArgs`]), fetch the args
/// artifact, decode it, and call its `path` accessor: `&TreePath` for reads,
/// `Option<&TreePath>` for lists and greps where `None` is the root or the
/// whole tree.
///
/// Seeded reads need no special case: an open builds them as ordinary
/// `tree.read` runs, so they appear in `read_inputs` like any other read. A
/// consumer subtracts the seeds of [`crate::session::OpenInput`] to get reads
/// outside seeds.
///
/// Fold from `Seq(0)`: starting mid-stream leaves in-flight chains
/// unattributed. Link entries are removed when consumed, so `links` holds
/// only the live frontier plus one terminal seq per rested activation, while
/// `sessions` accumulates per session across continues.
#[derive(Default)]
pub struct Reads {
    cursor: ViewCursor,
    links: BTreeMap<Seq, SessionKey>,
    sessions: BTreeMap<SessionKey, SessionReads>,
}

impl Reads {
    /// The file-access record of `key`, if it read or searched anything.
    #[must_use]
    pub fn session(&self, key: &SessionKey) -> Option<&SessionReads> {
        self.sessions.get(key)
    }
}

#[view(cursor = cursor)]
impl View for Reads {
    #[fold]
    fn opened(&mut self, run: Transition, at: At) {
        if run.program.name().as_str() != SessionOpen::NAME {
            return;
        }
        self.links.insert(at.seq, SessionKey::new(at.seq.0));
    }

    #[fold]
    fn continued(&mut self, run: Transition, cited: &Cited, at: At) {
        if run.program.name().as_str() != SessionContinue::NAME {
            return;
        }
        let Ok(input) = cited.get(Ref::<ContinueInput>::from_digest(run.input)) else {
            return;
        };
        self.links.insert(at.seq, input.session());
    }

    #[fold]
    fn requested(&mut self, request: Requested, at: At) {
        let RequestSource::Reaction { reactor, .. } = &request.source else {
            return;
        };
        if reactor.as_str() != MuseSession::NAMESPACE {
            return;
        }
        if let Some(key) = at.cause.and_then(|cause| self.links.remove(&cause)) {
            self.links.insert(at.seq, key);
        }
    }

    #[fold]
    fn ran(&mut self, run: Transition, at: At) {
        let Some(key) = at.cause.and_then(|cause| self.links.remove(&cause)) else {
            return;
        };
        self.links.insert(at.seq, key);
        let name = run.program.name().as_str();
        if name == TreeRead::NAME {
            self.sessions.entry(key).or_default().read.insert(run.input);
        } else if name == TreeList::NAME {
            self.sessions.entry(key).or_default().listed.insert(run.input);
        } else if name == TreeGrep::NAME {
            self.sessions.entry(key).or_default().searched.insert(run.input);
        }
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{
        ClosureArtifact, Entry, ProgramName, ProgramRef, ReactorName, RequestSource, Requested, RuleName, Seq,
        Transition,
    };
    use aether_bloomery_program::{Cited, ErasedTooled, Program, Reactor, View};
    use aether_bloomery_workspace::TreePath;
    use aether_data::{Cites, Digest, Kind, Ref, Storage, StorageData};

    use std::collections::BTreeMap;

    use super::Reads;
    use crate::session::fixture::{SmallTree, path, stored};
    use crate::session::{ContinueInput, MuseSession, SessionContinue, SessionKey, SessionOpen, TurnLimit};
    use crate::tools::{GrepArgs, ListArgs, ReadArgs, TreeEdit, TreeGrep, TreeList, TreeRead, VendorRead};

    fn bundle() -> Digest {
        Digest::from_bytes([9; 32])
    }

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn program_ref(name: &str) -> ProgramRef {
        ProgramRef::new(bundle(), ProgramName::new(name).expect("program name"))
    }

    fn transition(name: &str, input: Digest, result: Digest) -> Transition {
        Transition { program: program_ref(name), input, result }
    }

    fn entry_for<K: Storage + Clone>(seq: u64, cause: Option<u64>, event: &K) -> Entry {
        Entry {
            seq: Seq(seq),
            kind: K::ID,
            cause: cause.map(Seq),
            recorded_at_millis: 0,
            bytes: K::encode_storage(&StorageData::from_value(event.clone())).expect("entry encodes"),
        }
    }

    fn muse_requested(input: Digest) -> Requested {
        Requested {
            program: program_ref(TreeRead::NAME),
            input,
            source: RequestSource::Reaction {
                bundle: bundle(),
                reactor: ReactorName::new(MuseSession::NAMESPACE).expect("reactor"),
                rule: RuleName::new("call").expect("rule"),
                ordinal: 0,
            },
        }
    }

    fn foreign_requested(input: Digest) -> Requested {
        Requested {
            program: program_ref(TreeRead::NAME),
            input,
            source: RequestSource::Reaction {
                bundle: bundle(),
                reactor: ReactorName::new("other.reactor").expect("reactor"),
                rule: RuleName::new("call").expect("rule"),
                ordinal: 0,
            },
        }
    }

    fn staged<A>(small: &SmallTree, args: &A) -> (Digest, ClosureArtifact, Vec<ClosureArtifact>)
    where
        A: Storage + Clone + Cites,
    {
        let (tooled, closure) = small.call(args);
        let input = stored(&tooled);
        let digest = input.claimed().unverified();
        (digest, input, closure)
    }

    fn folded(entries: &[Entry], digests: &[Vec<Digest>], pool: &[ClosureArtifact]) -> Reads {
        let pages: Vec<Cited> = digests.iter().map(|entry| Cited::new(entry.clone(), pool)).collect();
        let mut reads = Reads::empty();
        reads.advance_cited(entries, &pages).expect("infallible folds never fail");
        reads
    }

    fn store_of(artifacts: Vec<ClosureArtifact>) -> BTreeMap<Digest, ClosureArtifact> {
        artifacts.into_iter().map(|artifact| (artifact.claimed().unverified(), artifact)).collect()
    }

    fn resolved<A>(store: &BTreeMap<Digest, ClosureArtifact>, digest: Digest) -> A
    where
        A: Storage + Kind,
    {
        let input_artifact = store.get(&digest).expect("input staged");
        let input_payload = input_artifact.load(digest).expect("bytes hash");
        let input: ErasedTooled = ErasedTooled::decode_storage(&input_payload).expect("tooled decodes").value;
        assert_eq!(input.args().kind(), A::ID);
        let args_ref = input.args();
        let args_artifact = store.get(&args_ref.digest()).expect("args staged");
        let args_payload = args_artifact.load(args_ref.digest()).expect("bytes hash");
        A::decode_storage(&args_payload).expect("args decode").value
    }

    #[test]
    fn interleaved_sessions_keep_their_own_reads_lists_and_greps() {
        // Catches cross-session misattribution, recording of a non-read program, and a dropped list or grep
        // category, with a continue re-anchoring one session under its same key.
        let small = SmallTree::new();
        let (read_digest, read_artifact, _) = staged(&small, &ReadArgs::new(path("README"), None, None));
        let (list_digest, list_artifact, _) = staged(&small, &ListArgs::new(Some(path("src"))));
        let (grep_digest, grep_artifact, _) = staged(&small, &GrepArgs::new("smelt", None, None));
        let (read_two_digest, read_two_artifact, _) = staged(&small, &ReadArgs::new(path("src/lib.rs"), None, None));
        let continued = ContinueInput::new(
            SessionKey::new(1),
            Ref::from_digest(digest(1)),
            None,
            None,
            TurnLimit::new(4).expect("limit"),
        );
        let continued_artifact = stored(&continued);
        let continued_digest = continued_artifact.claimed().unverified();
        let dummy = digest(7);

        let open_a = transition(SessionOpen::NAME, dummy, dummy);
        let open_b = transition(SessionOpen::NAME, dummy, dummy);
        let continued_run = transition(SessionContinue::NAME, continued_digest, dummy);
        let read_run = transition(TreeRead::NAME, read_digest, dummy);
        let list_run = transition(TreeList::NAME, list_digest, dummy);
        let grep_run = transition(TreeGrep::NAME, grep_digest, dummy);
        let edit_run = transition(TreeEdit::NAME, dummy, dummy);
        let read_two_run = transition(TreeRead::NAME, read_two_digest, dummy);

        let entries = vec![
            entry_for(1, None, &open_a),
            entry_for(2, None, &open_b),
            entry_for(3, Some(1), &muse_requested(dummy)),
            entry_for(4, Some(3), &read_run),
            entry_for(5, Some(2), &muse_requested(dummy)),
            entry_for(6, Some(5), &list_run),
            entry_for(7, None, &continued_run),
            entry_for(8, Some(7), &muse_requested(dummy)),
            entry_for(9, Some(8), &grep_run),
            entry_for(10, Some(6), &muse_requested(dummy)),
            entry_for(11, Some(10), &edit_run),
            entry_for(12, Some(11), &muse_requested(dummy)),
            entry_for(13, Some(12), &read_two_run),
        ];
        let reads = folded(
            &entries,
            &[
                vec![dummy, dummy],
                vec![dummy, dummy],
                vec![],
                vec![read_digest, dummy],
                vec![],
                vec![list_digest, dummy],
                vec![continued_digest, dummy],
                vec![],
                vec![grep_digest, dummy],
                vec![],
                vec![dummy, dummy],
                vec![],
                vec![read_two_digest, dummy],
            ],
            &[read_artifact, list_artifact, grep_artifact, read_two_artifact, continued_artifact],
        );

        let key_a = SessionKey::new(1);
        let key_b = SessionKey::new(2);
        let session_a = reads.session(&key_a).expect("session A read");
        let session_b = reads.session(&key_b).expect("session B read");
        assert!(session_a.read_inputs().contains(&read_digest), "{:?}", session_a.read_inputs());
        assert!(session_a.grep_inputs().contains(&grep_digest), "{:?}", session_a.grep_inputs());
        assert!(session_a.list_inputs().is_empty(), "{:?}", session_a.list_inputs());
        assert!(session_b.list_inputs().contains(&list_digest), "{:?}", session_b.list_inputs());
        assert!(session_b.read_inputs().contains(&read_two_digest), "{:?}", session_b.read_inputs());
        assert!(session_b.grep_inputs().is_empty(), "{:?}", session_b.grep_inputs());
        assert!(!session_a.read_inputs().contains(&read_two_digest), "sessions keep their own reads");
        assert!(!session_b.read_inputs().contains(&read_digest), "sessions keep their own reads");
    }

    #[test]
    fn foreign_reactor_and_vendor_runs_leave_no_reads() {
        // Catches a missing reactor filter or an over-broad program match: a tree.read under a foreign reactor and
        // a vendor.read on the linked chain must both leave the session without reads.
        let small = SmallTree::new();
        let (read_digest, read_artifact, _) = staged(&small, &ReadArgs::new(path("README"), None, None));
        let (vendor_tooled, _) = small.vendor_call(&ReadArgs::new(path("README"), None, None));
        let vendor_artifact = stored(&vendor_tooled);
        let vendor_digest = vendor_artifact.claimed().unverified();
        let dummy = digest(7);

        let open = transition(SessionOpen::NAME, dummy, dummy);
        let foreign = foreign_requested(dummy);
        let read_run = transition(TreeRead::NAME, read_digest, dummy);
        let vendor_run = transition(VendorRead::NAME, vendor_digest, dummy);

        let reads = folded(
            &[
                entry_for(1, None, &open),
                entry_for(2, Some(1), &foreign),
                entry_for(3, Some(2), &read_run),
                entry_for(4, Some(1), &muse_requested(dummy)),
                entry_for(5, Some(4), &vendor_run),
            ],
            &[vec![dummy, dummy], vec![], vec![read_digest, dummy], vec![], vec![vendor_digest, dummy]],
            &[read_artifact, vendor_artifact],
        );

        assert_eq!(reads.session(&SessionKey::new(1)), None, "foreign and vendor runs are not reads");
    }

    #[test]
    fn recorded_digests_resolve_to_their_paths() {
        // Catches accessor or kind confusion and proves the documented recipe: every recorded digest decodes
        // through its staged tooled input to its args and path, including root and whole-tree nones.
        let small = SmallTree::new();
        let (read_digest, read_input, read_closure) = staged(&small, &ReadArgs::new(path("README"), None, None));
        let (list_root_digest, list_root_input, list_root_closure) = staged(&small, &ListArgs::new(None));
        let (list_src_digest, list_src_input, list_src_closure) = staged(&small, &ListArgs::new(Some(path("src"))));
        let (grep_all_digest, grep_all_input, grep_all_closure) = staged(&small, &GrepArgs::new("smelt", None, None));
        let (grep_src_digest, grep_src_input, grep_src_closure) =
            staged(&small, &GrepArgs::new("smelt", Some(path("src")), None));
        let dummy = digest(7);

        let reads = folded(
            &[
                entry_for(1, None, &transition(SessionOpen::NAME, dummy, dummy)),
                entry_for(2, Some(1), &muse_requested(dummy)),
                entry_for(3, Some(2), &transition(TreeRead::NAME, read_digest, dummy)),
                entry_for(4, Some(3), &muse_requested(dummy)),
                entry_for(5, Some(4), &transition(TreeList::NAME, list_root_digest, dummy)),
                entry_for(6, Some(5), &muse_requested(dummy)),
                entry_for(7, Some(6), &transition(TreeList::NAME, list_src_digest, dummy)),
                entry_for(8, Some(7), &muse_requested(dummy)),
                entry_for(9, Some(8), &transition(TreeGrep::NAME, grep_all_digest, dummy)),
                entry_for(10, Some(9), &muse_requested(dummy)),
                entry_for(11, Some(10), &transition(TreeGrep::NAME, grep_src_digest, dummy)),
            ],
            &[
                vec![dummy, dummy],
                vec![],
                vec![read_digest, dummy],
                vec![],
                vec![list_root_digest, dummy],
                vec![],
                vec![list_src_digest, dummy],
                vec![],
                vec![grep_all_digest, dummy],
                vec![],
                vec![grep_src_digest, dummy],
            ],
            &[
                read_input.clone(),
                list_root_input.clone(),
                list_src_input.clone(),
                grep_all_input.clone(),
                grep_src_input.clone(),
            ],
        );

        let session = reads.session(&SessionKey::new(1)).expect("session read");
        assert!(session.read_inputs().contains(&read_digest));
        assert!(session.list_inputs().contains(&list_root_digest));
        assert!(session.list_inputs().contains(&list_src_digest));
        assert!(session.grep_inputs().contains(&grep_all_digest));
        assert!(session.grep_inputs().contains(&grep_src_digest));

        let store = store_of(
            read_closure
                .into_iter()
                .chain([read_input])
                .chain(list_root_closure)
                .chain([list_root_input])
                .chain(list_src_closure)
                .chain([list_src_input])
                .chain(grep_all_closure)
                .chain([grep_all_input])
                .chain(grep_src_closure)
                .chain([grep_src_input])
                .collect(),
        );
        let read_args: ReadArgs = resolved(&store, read_digest);
        assert_eq!(read_args.path().as_str(), "README");
        for (digest, expected) in [(list_root_digest, None), (list_src_digest, Some("src"))] {
            let args: ListArgs = resolved(&store, digest);
            assert_eq!(args.path().map(TreePath::as_str), expected);
        }
        for (digest, expected) in [(grep_all_digest, None), (grep_src_digest, Some("src"))] {
            let args: GrepArgs = resolved(&store, digest);
            assert_eq!(args.path().map(TreePath::as_str), expected);
        }
    }

    #[test]
    fn a_continue_without_its_input_advances_without_linking() {
        // Catches view poisoning: a continued run whose input is not cited never fails the fold, and a later read
        // on its chain stays unattributed.
        let key = SessionKey::new(42);
        let continued =
            ContinueInput::new(key, Ref::from_digest(digest(1)), None, None, TurnLimit::new(4).expect("limit"));
        let continued_digest = stored(&continued).claimed().unverified();
        let dummy = digest(7);
        let continued_run = transition(SessionContinue::NAME, continued_digest, dummy);
        let entry = entry_for(1, None, &continued_run);

        let mut reads = Reads::empty();
        reads.advance(&[entry]).expect("infallible folds never fail");
        assert_eq!(reads.cursor(), Seq(1));
        assert_eq!(reads.session(&key), None);

        let small = SmallTree::new();
        let (read_digest, _, _) = staged(&small, &ReadArgs::new(path("README"), None, None));
        let read_run = transition(TreeRead::NAME, read_digest, dummy);
        let follow = vec![entry_for(2, Some(1), &muse_requested(dummy)), entry_for(3, Some(2), &read_run)];

        reads.advance(&follow).expect("infallible folds never fail");
        assert_eq!(reads.cursor(), Seq(3));
        assert_eq!(reads.session(&key), None, "the uncited chain stays unattributed");
    }
}
