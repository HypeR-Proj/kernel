// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Model checks for linear scheduler residence and independent switch-tail ownership.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Residence {
    Coordinator,
    Cpu(usize),
}

fn lock_required(owner: Residence, active: Option<usize>) -> Result<Option<usize>, ()> {
    match (owner, active) {
        (Residence::Coordinator, _) => Ok(None),
        (Residence::Cpu(cpu), None) => Ok(Some(cpu)),
        (Residence::Cpu(cpu), Some(active)) if cpu == active => Ok(None),
        (Residence::Cpu(_), Some(_)) => Err(()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Model {
    residence: Residence,
    ready: bool,
    current: bool,
    switching_from: Option<u64>,
}

impl Model {
    const fn new() -> Self {
        Self {
            residence: Residence::Coordinator,
            ready: false,
            current: false,
            switching_from: None,
        }
    }

    fn enqueue(&mut self, cpu: usize) -> Result<(), ()> {
        let owner_valid = match self.residence {
            Residence::Coordinator => true,
            Residence::Cpu(owner) => owner == cpu,
        };
        if !owner_valid || self.ready || self.current {
            return Err(());
        }
        self.residence = Residence::Cpu(cpu);
        self.ready = true;
        Ok(())
    }

    fn dequeue(&mut self, cpu: usize) -> Result<(), ()> {
        if self.residence != Residence::Cpu(cpu) || !self.ready || self.current {
            return Err(());
        }
        self.ready = false;
        Ok(())
    }

    fn install_current(&mut self, cpu: usize) -> Result<(), ()> {
        if self.residence != Residence::Cpu(cpu) || self.ready || self.current {
            return Err(());
        }
        self.current = true;
        Ok(())
    }

    fn release_cpu(&mut self, cpu: usize) -> Result<(), ()> {
        if self.residence != Residence::Cpu(cpu)
            || self.ready
            || self.current
            || self.switching_from.is_some()
        {
            return Err(());
        }
        self.residence = Residence::Coordinator;
        Ok(())
    }
}

#[test]
fn ready_and_current_share_one_cpu_domain() {
    let mut model = Model::new();
    assert_eq!(model.enqueue(2), Ok(()));
    assert_eq!(model.enqueue(2), Err(()));
    assert_eq!(model.dequeue(1), Err(()));
    assert_eq!(model.residence, Residence::Cpu(2));
    assert_eq!(model.dequeue(2), Ok(()));
    assert_eq!(model.install_current(2), Ok(()));
    assert_eq!(model.release_cpu(2), Err(()));
    model.current = false;
    assert_eq!(model.release_cpu(1), Err(()));
    assert_eq!(model.residence, Residence::Cpu(2));
    assert_eq!(model.release_cpu(2), Ok(()));
}

#[test]
fn switching_context_does_not_duplicate_schedule_ownership() {
    let mut outgoing = Model {
        residence: Residence::Cpu(0),
        ready: true,
        current: false,
        switching_from: Some(41),
    };
    let mut incoming = Model::new();

    assert_eq!(incoming.enqueue(0), Ok(()));
    assert_eq!(incoming.dequeue(0), Ok(()));
    assert_eq!(incoming.install_current(0), Ok(()));
    assert_eq!(outgoing.residence, Residence::Cpu(0));
    assert_eq!(incoming.residence, Residence::Cpu(0));
    assert_eq!(outgoing.release_cpu(0), Err(()));
    assert_eq!(outgoing.switching_from.take(), Some(41));
    assert_eq!(outgoing.switching_from, None);
}

#[test]
fn matching_active_cpu_reaches_the_operation_body() {
    assert_eq!(lock_required(Residence::Cpu(2), None), Ok(Some(2)));
    assert_eq!(lock_required(Residence::Cpu(2), Some(2)), Ok(None));
    assert_eq!(lock_required(Residence::Cpu(2), Some(1)), Err(()));
    assert_eq!(lock_required(Residence::Coordinator, Some(2)), Ok(None));
}

#[test]
fn control_queue_links_cross_cpu_owned_neighbors() {
    #[derive(Clone, Copy)]
    enum ControlMembership {
        Waiting,
        Terminated,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ControlLinks {
        previous: Option<usize>,
        next: Option<usize>,
    }

    struct Node {
        schedule_owner: Residence,
        membership: ControlMembership,
        control: ControlLinks,
    }

    for membership in [ControlMembership::Waiting, ControlMembership::Terminated] {
        let mut nodes = [
            Node {
                schedule_owner: Residence::Cpu(0),
                membership,
                control: ControlLinks {
                    previous: None,
                    next: Some(1),
                },
            },
            Node {
                schedule_owner: Residence::Cpu(1),
                membership,
                control: ControlLinks {
                    previous: Some(0),
                    next: None,
                },
            },
        ];

        // TransitionLock owns both control cells and therefore removes either
        // node without acquiring or interpreting either CPU schedule token.
        let removed = nodes[0].control;
        nodes[1].control.previous = removed.previous;
        nodes[0].control = ControlLinks {
            previous: None,
            next: None,
        };

        assert_eq!(nodes[0].schedule_owner, Residence::Cpu(0));
        assert_eq!(nodes[1].schedule_owner, Residence::Cpu(1));
        assert!(matches!(
            (nodes[0].membership, nodes[1].membership),
            (ControlMembership::Waiting, ControlMembership::Waiting)
                | (ControlMembership::Terminated, ControlMembership::Terminated)
        ));
        assert_eq!(nodes[0].control.next, None);
        assert_eq!(nodes[1].control.previous, None);
    }
}

#[test]
fn ready_and_control_queue_authorities_are_disjoint() {
    #[derive(Clone, Copy)]
    enum Membership {
        Ready(usize),
        Waiting,
        Terminated,
    }

    fn insertion_allowed(owner: Residence, active: usize, membership: Membership) -> bool {
        match membership {
            Membership::Ready(target) => match owner {
                Residence::Coordinator => true,
                Residence::Cpu(cpu) => cpu == active && target == cpu,
            },
            // TransitionLock-owned control links are independent of schedule
            // residence, including a foreign CPU-owned current thread.
            Membership::Waiting | Membership::Terminated => true,
        }
    }

    assert!(insertion_allowed(Residence::Cpu(1), 1, Membership::Waiting));
    assert!(insertion_allowed(
        Residence::Cpu(1),
        1,
        Membership::Terminated
    ));
    assert!(insertion_allowed(
        Residence::Cpu(1),
        1,
        Membership::Ready(1)
    ));
    assert!(!insertion_allowed(
        Residence::Cpu(1),
        1,
        Membership::Ready(0)
    ));
    assert!(insertion_allowed(Residence::Cpu(2), 1, Membership::Waiting));
}

#[test]
fn ready_vcpu_is_not_reported_as_running() {
    #[derive(Clone, Copy)]
    enum State {
        Ready,
        Running,
    }

    fn running_cpu(state: State, owner: usize, current: bool) -> Result<Option<usize>, ()> {
        match (state, current) {
            (State::Running, true) => Ok(Some(owner)),
            (State::Running, false) => Err(()),
            (State::Ready, _) => Ok(None),
        }
    }

    assert_eq!(running_cpu(State::Ready, 2, false), Ok(None));
    assert_eq!(running_cpu(State::Running, 2, true), Ok(Some(2)));
    assert_eq!(running_cpu(State::Running, 2, false), Err(()));
}

#[test]
fn cpu_owned_terminal_node_waits_for_switch_tail() {
    fn detachable(owner: Residence, context_stopped: bool) -> bool {
        owner == Residence::Coordinator && context_stopped
    }

    assert!(!detachable(Residence::Cpu(0), false));
    assert!(!detachable(Residence::Cpu(0), true));
    assert!(detachable(Residence::Coordinator, true));
}

#[test]
fn blocked_stop_uses_the_pre_resolution_snapshot() {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum State {
        Blocked,
        Ready,
    }

    struct StopModel {
        residence: Residence,
        state: State,
    }

    impl StopModel {
        fn resolve_queued_wait(&mut self, target: usize) -> usize {
            assert_eq!(self.residence, Residence::Coordinator);
            assert_eq!(self.state, State::Blocked);
            self.residence = Residence::Cpu(target);
            self.state = State::Ready;
            target
        }

        fn coordinator_state(&self) -> Result<State, ()> {
            (self.residence == Residence::Coordinator)
                .then_some(self.state)
                .ok_or(())
        }
    }

    let mut thread = StopModel {
        residence: Residence::Coordinator,
        state: State::Blocked,
    };
    let state_before_resolution = thread.coordinator_state();
    let ready_cpu = thread.resolve_queued_wait(1);

    // The ownership transfer deliberately makes a second coordinator read
    // invalid. Stop routing is instead determined by the snapshot and the
    // resolver-owned ready publication.
    assert_eq!(thread.coordinator_state(), Err(()));
    assert_eq!(state_before_resolution, Ok(State::Blocked));
    assert_eq!(ready_cpu, 1);
}

#[test]
fn source_to_target_admission_never_nests_cpu_locks() {
    struct AdmissionModel {
        schedulable: [bool; 3],
        active_cpu: Option<usize>,
        cpu_lock_depth: [usize; 3],
    }

    impl AdmissionModel {
        fn target_is_schedulable(&self, cpu: usize) -> bool {
            self.schedulable[cpu]
        }
    }

    let model = AdmissionModel {
        schedulable: [true, true, false],
        active_cpu: Some(0),
        cpu_lock_depth: [1, 0, 0],
    };
    assert!(model.target_is_schedulable(1));
    assert!(!model.target_is_schedulable(2));
    assert_eq!(model.active_cpu, Some(0));
    assert_eq!(model.cpu_lock_depth, [1, 0, 0]);
}

#[test]
fn remote_migration_between_local_prepare_and_tail_goes_slow() {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Tail {
        Complete,
        NeedsCoordinator,
    }

    fn tail(pending_migration: bool, generation: u64, observed: u64) -> Tail {
        if pending_migration || generation != observed {
            Tail::NeedsCoordinator
        } else {
            Tail::Complete
        }
    }

    assert_eq!(tail(false, 7, 7), Tail::Complete);
    assert_eq!(tail(true, 7, 7), Tail::NeedsCoordinator);
    assert_eq!(tail(false, 8, 7), Tail::NeedsCoordinator);
}

#[test]
fn coordinated_wait_and_exit_switches_never_complete_locally() {
    #[derive(Clone, Copy)]
    enum Disposition {
        Local,
        Coordinated,
    }

    let completes_locally = |disposition| matches!(disposition, Disposition::Local);
    assert!(completes_locally(Disposition::Local));
    assert!(!completes_locally(Disposition::Coordinated));
}

#[test]
fn local_policy_keeps_fifo_and_fair_ordering() {
    #[derive(Clone, Copy)]
    enum Policy {
        Fifo(u8),
        Fair,
    }

    fn may_yield(current: Policy, ready: Policy) -> bool {
        match (current, ready) {
            (Policy::Fair, _) => true,
            (Policy::Fifo(current), Policy::Fifo(ready)) => ready <= current,
            (Policy::Fifo(_), Policy::Fair) => false,
        }
    }

    assert!(may_yield(Policy::Fair, Policy::Fair));
    assert!(may_yield(Policy::Fifo(10), Policy::Fifo(9)));
    assert!(may_yield(Policy::Fifo(10), Policy::Fifo(10)));
    assert!(!may_yield(Policy::Fifo(10), Policy::Fifo(11)));
    assert!(!may_yield(Policy::Fifo(10), Policy::Fair));
}

#[test]
fn coordinator_fallback_releases_cpu_lock_first() {
    #[derive(Default)]
    struct Locks {
        cpu: bool,
        transition: bool,
    }

    let mut locks = Locks {
        cpu: true,
        ..Locks::default()
    };
    let needs_coordinator = true;
    locks.cpu = false;
    if needs_coordinator {
        assert!(!locks.cpu);
        locks.transition = true;
    }
    assert!(locks.transition);
    assert!(!locks.cpu);
}

#[test]
fn retirement_slots_bound_exit_bursts_until_reaper_completion() {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Slot {
        Occupied,
        RetiringOwned,
        RetiringTaken,
        Vacant,
    }

    let mut slots = [Slot::Occupied; 4];
    let mut queue = std::collections::VecDeque::new();
    for (id, slot) in slots.iter_mut().enumerate() {
        *slot = Slot::RetiringOwned;
        queue.push_back(id);
    }
    assert_eq!(queue.len(), slots.len());
    assert!(!slots.contains(&Slot::Vacant));

    while let Some(id) = queue.pop_front() {
        assert_eq!(slots[id], Slot::RetiringOwned);
        slots[id] = Slot::RetiringTaken;
        // Lock-external destruction cannot make this identity reusable.
        assert_eq!(slots[id], Slot::RetiringTaken);
        slots[id] = Slot::Vacant;
    }
    assert!(slots.iter().all(|slot| *slot == Slot::Vacant));
}

#[test]
fn bootstrap_retirement_keeps_slot_zero_out_of_reuse() {
    let mut slots = [false; 3];
    let mut free = Vec::new();

    // Retirement completion makes every identity absent, but only ordinary
    // slots enter the allocator's reusable namespace.
    slots[0] = true;
    slots[1] = true;
    for slot in [0usize, 1] {
        slots[slot] = false;
        if slot != 0 {
            free.push(slot);
        }
    }

    assert_eq!(slots, [false, false, false]);
    assert_eq!(free, [1]);
    assert!(!free.contains(&0));
}

#[test]
fn generation_is_reaped_only_after_retiring_slot_completion() {
    #[derive(Clone, Copy)]
    enum Slot {
        Occupied(u64),
        Retiring(u64),
        Vacant,
    }

    let reaped = |slot: Slot, generation: u64| match slot {
        Slot::Occupied(current) | Slot::Retiring(current) if current == generation => false,
        Slot::Occupied(_) | Slot::Retiring(_) | Slot::Vacant => true,
    };

    assert!(!reaped(Slot::Occupied(41), 41));
    assert!(!reaped(Slot::Retiring(41), 41));
    assert!(reaped(Slot::Vacant, 41));
    assert!(reaped(Slot::Occupied(42), 41));
}

#[test]
fn retirement_publication_closes_worker_sleep_races() {
    use hyper::sync::{DeferredWork, WorkDisposition};

    let work = DeferredWork::new();
    assert!(work.claim_initial_worker());
    work.begin_batch();
    assert_eq!(work.finish_batch(false), WorkDisposition::Wait);

    // A producer linearized after ownership release elects an IRQ prompt.
    assert!(work.request());
    assert!(work.consume_prompt());
    assert!(work.claim_notification());

    // A producer racing an active batch leaves durable pending work; the
    // worker cannot release ownership and sleep past it.
    work.begin_batch();
    assert!(!work.request());
    assert_eq!(work.finish_batch(false), WorkDisposition::Continue);
}

#[test]
fn retirement_work_survives_publication_before_worker_and_irq_readiness() {
    use hyper::sync::{DeferredWork, WorkDisposition};

    let work = DeferredWork::new();

    // Early producers cannot notify hardware, but both the work and elected
    // prompt remain sticky until startup publishes the worker and IRQ route.
    assert!(work.request());
    assert!(work.claim_initial_worker());
    assert!(work.consume_prompt());
    assert!(!work.claim_notification());

    work.begin_batch();
    assert_eq!(work.finish_batch(false), WorkDisposition::Wait);
    assert!(!work.consume_prompt());
}

#[test]
fn prepublication_reaper_prompt_does_not_suppress_later_work() {
    use hyper::sync::{DeferredWork, WorkDisposition};

    let work = DeferredWork::new();

    // An early object rollback publishes work before the worker or IRQ prompt
    // route exists. Initial ownership consumes that unissued prompt before
    // making the worker runnable.
    assert!(work.request());
    assert!(work.claim_initial_worker());
    assert!(work.consume_prompt());
    work.begin_batch();
    assert_eq!(work.finish_batch(false), WorkDisposition::Wait);

    // Once the initial batch sleeps, a new producer must be able to elect a
    // fresh prompt and transfer wake ownership through IRQ service.
    assert!(work.request());
    assert!(work.consume_prompt());
    assert!(work.claim_notification());
}

#[test]
fn irq_prompt_enable_wakes_an_initial_worker_that_already_waited() {
    use hyper::sync::{DeferredWork, WorkDisposition};

    let work = DeferredWork::new();
    assert!(work.request());
    assert!(work.claim_initial_worker());

    // Model the worker draining before IRQ prompting is enabled. Its original
    // unissued prompt remains set while it releases wake ownership.
    work.begin_batch();
    assert_eq!(work.finish_batch(false), WorkDisposition::Wait);

    // A producer cannot elect behind the stale prompt. Enabling the prompt
    // route must consume it and claim the pending notification directly.
    assert!(!work.request());
    assert!(work.consume_prompt());
    assert!(work.claim_notification());
}
