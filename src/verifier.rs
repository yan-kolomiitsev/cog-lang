//! Bounded verification of frame shape, value flow, callable contracts, and local initialization.
//! Dynamic and Error-dependent value checks remain interpreter responsibilities.
use crate::backend::{CallableSignature, CompiledProgram};
use crate::bytecode::LIMIT;
use crate::instruction::Instruction;
use crate::template::ArrayTemplate;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

// Bound analysis cost independently of the size of an adversarial CFG. These
// are verifier limits, not runtime instruction or OS permission budgets.
const MAX_FACT_WORK: usize = 4_000_000;
const MAX_VISITS: usize = 4_000_000;
const MAX_TRACKED_AGGREGATES: usize = 128;
const MAX_TRACKED_AGGREGATE_ITEMS: usize = 32;
const MAX_TRACKED_CLOSURE_TARGETS: usize = 128;
const MAX_SUMMARIZED_CALLABLES: usize = 128;
const MAX_SUMMARY_INSTRUCTIONS: usize = 128;
const MAX_SUMMARY_STACK: usize = 32;

#[derive(Clone)]
struct State {
    height: usize,
    initialized: Rc<BTreeSet<usize>>,
    stack_values: Rc<BTreeMap<usize, FlowValue>>,
    aggregate_shapes: Rc<BTreeMap<usize, Rc<Vec<FlowValue>>>>,
    record_shapes: Rc<BTreeMap<usize, Rc<BTreeMap<u32, FlowValue>>>>,
    local_values: Rc<BTreeMap<usize, FlowValue>>,
    global_values: Rc<BTreeMap<usize, FlowValue>>,
    capture_values: Option<Rc<Vec<FlowValue>>>,
}

struct VerifyContext<'a> {
    program: &'a CompiledProgram,
    instruction_string_ids: &'a [Option<u32>],
    strings: &'a [Rc<str>],
    callable_results: &'a BTreeMap<usize, FlowValue>,
    write_free_callables: &'a BTreeSet<usize>,
}

struct FlowAnalysis<'a> {
    capture_values: Option<&'a [FlowValue]>,
    closure_outputs: &'a mut BTreeMap<usize, Vec<FlowValue>>,
    fact_work: &'a mut usize,
    visits: &'a mut usize,
}

#[derive(Debug)]
struct InternedStrings {
    instruction_ids: Vec<Option<u32>>,
    values: Vec<Rc<str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CallableTarget {
    entry: usize,
    captures: usize,
}

const TYPE_INT: u16 = 1 << 0;
const TYPE_FLOAT: u16 = 1 << 1;
const TYPE_BOOL: u16 = 1 << 2;
const TYPE_NIL: u16 = 1 << 3;
const TYPE_STR: u16 = 1 << 4;
const TYPE_ARRAY: u16 = 1 << 5;
const TYPE_TUPLE: u16 = 1 << 6;
const TYPE_RECORD: u16 = 1 << 7;
const TYPE_VARIANT: u16 = 1 << 8;
const TYPE_FUNCTION: u16 = 1 << 9;
const TYPE_OTHER: u16 = 1 << 10;
const TYPE_STRBUF: u16 = 1 << 11;
const TYPE_TYPE_TAG: u16 = 1 << 12;
const ALL_VALUE_TYPES: u16 = (1 << 13) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FlowValue {
    normal_types: u16,
    may_error: bool,
    callable: Option<CallableTarget>,
    // TypeTag is constructor metadata; it is not a tagged Variant value.
    is_type_tag: bool,
    // Interned exact tag for metadata or a statically constructed value.
    known_tag: Option<u32>,
    known_bool: Option<bool>,
    known_int: Option<i64>,
    known_string: Option<u32>,
}

impl FlowValue {
    const UNKNOWN: Self = Self {
        normal_types: ALL_VALUE_TYPES,
        may_error: true,
        callable: None,
        is_type_tag: false,
        known_tag: None,
        known_bool: None,
        known_int: None,
        known_string: None,
    };

    const ERROR: Self = Self {
        normal_types: 0,
        may_error: true,
        callable: None,
        is_type_tag: false,
        known_tag: None,
        known_bool: None,
        known_int: None,
        known_string: None,
    };

    const fn normal(normal_types: u16) -> Self {
        Self {
            normal_types,
            may_error: false,
            callable: None,
            is_type_tag: false,
            known_tag: None,
            known_bool: None,
            known_int: None,
            known_string: None,
        }
    }

    const fn recoverable(normal_types: u16) -> Self {
        Self {
            normal_types,
            may_error: true,
            callable: None,
            is_type_tag: false,
            known_tag: None,
            known_bool: None,
            known_int: None,
            known_string: None,
        }
    }

    const fn boolean(value: bool) -> Self {
        Self {
            normal_types: TYPE_BOOL,
            may_error: false,
            callable: None,
            is_type_tag: false,
            known_tag: None,
            known_bool: Some(value),
            known_int: None,
            known_string: None,
        }
    }

    const fn integer(value: i64) -> Self {
        Self {
            normal_types: TYPE_INT,
            may_error: false,
            callable: None,
            is_type_tag: false,
            known_tag: None,
            known_bool: None,
            known_int: Some(value),
            known_string: None,
        }
    }

    const fn string(value: u32) -> Self {
        Self {
            normal_types: TYPE_STR,
            may_error: false,
            callable: None,
            is_type_tag: false,
            known_tag: None,
            known_bool: None,
            known_int: None,
            known_string: Some(value),
        }
    }

    fn is_unknown(self) -> bool {
        self == Self::UNKNOWN
    }

    const fn type_tag() -> Self {
        Self {
            normal_types: TYPE_TYPE_TAG,
            may_error: false,
            callable: None,
            is_type_tag: true,
            known_tag: None,
            known_bool: None,
            known_int: None,
            known_string: None,
        }
    }

    fn join(self, other: Self) -> Self {
        let may_error = self.may_error || other.may_error;
        Self {
            normal_types: self.normal_types | other.normal_types,
            may_error,
            is_type_tag: self.is_type_tag && other.is_type_tag && !may_error,
            known_tag: (self.known_tag == other.known_tag)
                .then_some(self.known_tag)
                .flatten(),
            callable: (self.callable == other.callable && !self.may_error && !other.may_error)
                .then_some(self.callable)
                .flatten(),
            known_bool: (self.known_bool == other.known_bool && !may_error)
                .then_some(self.known_bool)
                .flatten(),
            known_int: (self.known_int == other.known_int && !may_error)
                .then_some(self.known_int)
                .flatten(),
            known_string: (self.known_string == other.known_string && !may_error)
                .then_some(self.known_string)
                .flatten(),
        }
    }
}

fn join_value_maps(
    left: &BTreeMap<usize, FlowValue>,
    right: &BTreeMap<usize, FlowValue>,
) -> BTreeMap<usize, FlowValue> {
    left.iter()
        .filter_map(|(slot, value)| {
            right
                .get(slot)
                .map(|other| (*slot, value.join(*other)))
                .filter(|(_, joined)| !joined.is_unknown())
        })
        .collect()
}

fn aggregate_shape_cost(facts: &BTreeMap<usize, Rc<Vec<FlowValue>>>) -> usize {
    facts
        .values()
        .fold(facts.len(), |cost, shape| cost.saturating_add(shape.len()))
}

fn join_aggregate_shapes(
    left: &BTreeMap<usize, Rc<Vec<FlowValue>>>,
    right: &BTreeMap<usize, Rc<Vec<FlowValue>>>,
) -> BTreeMap<usize, Rc<Vec<FlowValue>>> {
    left.iter()
        .filter_map(|(slot, shape)| {
            let other = right.get(slot)?;
            if shape.len() != other.len() {
                return None;
            }
            let joined: Vec<_> = shape
                .iter()
                .zip(other.iter())
                .map(|(left, right)| left.join(*right))
                .collect();
            (joined.is_empty() || joined.iter().any(|value| !value.is_unknown()))
                .then(|| (*slot, Rc::new(joined)))
        })
        .collect()
}

fn record_shape_cost(facts: &BTreeMap<usize, Rc<BTreeMap<u32, FlowValue>>>) -> usize {
    facts
        .values()
        .fold(facts.len(), |cost, shape| cost.saturating_add(shape.len()))
}

fn join_record_shapes(
    left: &BTreeMap<usize, Rc<BTreeMap<u32, FlowValue>>>,
    right: &BTreeMap<usize, Rc<BTreeMap<u32, FlowValue>>>,
) -> BTreeMap<usize, Rc<BTreeMap<u32, FlowValue>>> {
    left.iter()
        .filter_map(|(slot, shape)| {
            let other = right.get(slot)?;
            let mut joined = BTreeMap::new();
            for (field, value) in shape.iter() {
                joined.insert(
                    *field,
                    value.join(other.get(field).copied().unwrap_or(FlowValue::ERROR)),
                );
            }
            for (field, value) in other.iter() {
                if !shape.contains_key(field) {
                    joined.insert(*field, FlowValue::ERROR.join(*value));
                }
            }
            (joined.len() <= MAX_TRACKED_AGGREGATE_ITEMS).then(|| (*slot, Rc::new(joined)))
        })
        .collect()
}

fn failure(pc: usize, message: &str) -> String {
    format!("bytecode verification at instruction {pc}: {message}")
}

fn signature(program: &CompiledProgram, target: usize) -> Result<&CallableSignature, String> {
    program
        .callables
        .get(&target)
        .ok_or_else(|| failure(target, "target is not a declared callable entry"))
}

fn function_constant(program: &CompiledProgram, value: &Value) -> Result<(), String> {
    if let Value::Function(target) = value
        && signature(program, *target)?.captures != 0
    {
        return Err(failure(*target, "function constant requires captures"));
    }
    Ok(())
}

fn template_functions(program: &CompiledProgram, template: &ArrayTemplate) -> Result<(), String> {
    match template {
        ArrayTemplate::Scalar(value) => function_constant(program, value),
        ArrayTemplate::Array(items) => {
            for item in items {
                template_functions(program, &item.template)?;
            }
            Ok(())
        }
    }
}

pub(crate) fn verify(program: &CompiledProgram) -> Result<(), String> {
    for (entry, callable) in &program.callables {
        if *entry == 0
            || *entry >= program.instructions.len()
            || callable.parameters > LIMIT
            || callable.captures > LIMIT
            || callable
                .parameter_types
                .as_ref()
                .is_some_and(|types| types.len() != callable.parameters)
        {
            return Err(failure(*entry, "invalid callable descriptor"));
        }
    }
    // Check references even in unreachable instructions, before graph traversal.
    for (pc, instruction) in program.instructions.iter().enumerate() {
        instruction
            .stack_effect(&program.imports)
            .map_err(|error| failure(pc, &error.to_string()))?;
        match instruction {
            Instruction::Call(target, parameters) => {
                let callable = signature(program, *target)?;
                if callable.parameters != *parameters || callable.captures != 0 {
                    return Err(failure(pc, "direct call signature mismatch"));
                }
            }
            Instruction::MakeClosure(target, captures)
                if signature(program, *target)?.captures != *captures =>
            {
                return Err(failure(pc, "closure capture signature mismatch"));
            }
            Instruction::MakeClosure(_, _) => {}
            Instruction::Push(value) => function_constant(program, value)?,
            Instruction::PushArray(template) => template_functions(program, template)?,
            _ => {}
        }
    }
    let mut fact_work = MAX_FACT_WORK;
    let interned_strings = intern_instruction_strings(program, &mut fact_work)?;
    let write_free_callables = write_free_callables(program, &mut fact_work)?;
    let callable_results = callable_result_summaries(
        program,
        &interned_strings.instruction_ids,
        &interned_strings.values,
        &write_free_callables,
        &mut fact_work,
    )?;
    let context = VerifyContext {
        program,
        instruction_string_ids: &interned_strings.instruction_ids,
        strings: &interned_strings.values,
        callable_results: &callable_results,
        write_free_callables: &write_free_callables,
    };
    let mut visits = MAX_VISITS;
    let mut capture_facts = BTreeMap::<usize, Vec<FlowValue>>::new();
    // A closure body may construct another closure, so summaries flow across
    // callable regions until stable. All sweeps share the verifier work budgets.
    loop {
        let mut next_capture_facts = BTreeMap::new();
        let mut start = 0;
        let mut parameters = 0;
        let mut captures = 0;
        for (end, next) in program
            .callables
            .iter()
            .map(|(entry, callable)| (*entry, Some(callable)))
            .chain(std::iter::once((program.instructions.len(), None)))
        {
            let mut flow = FlowAnalysis {
                capture_values: capture_facts.get(&start).map(Vec::as_slice),
                closure_outputs: &mut next_capture_facts,
                fact_work: &mut fact_work,
                visits: &mut visits,
            };
            region(&context, start, end, parameters, captures, &mut flow)?;
            start = end;
            if let Some(callable) = next {
                parameters = callable.parameters;
                captures = callable.captures;
            }
        }
        if next_capture_facts == capture_facts {
            break;
        }
        capture_facts = next_capture_facts;
    }
    Ok(())
}

fn intern_instruction_strings(
    program: &CompiledProgram,
    fact_work: &mut usize,
) -> Result<InternedStrings, String> {
    let mut ids = HashMap::<Rc<str>, u32>::new();
    let mut instruction_ids = Vec::new();
    instruction_ids
        .try_reserve_exact(program.instructions.len())
        .map_err(|_| failure(0, "cannot allocate verifier instruction string table"))?;
    instruction_ids.resize(program.instructions.len(), None);
    let mut strings = Vec::<Rc<str>>::new();

    for (pc, instruction) in program.instructions.iter().enumerate() {
        let value = match instruction {
            Instruction::Push(Value::Str(value) | Value::TypeTag(value))
            | Instruction::Push(Value::Variant(value, _)) => Some(value),
            _ => None,
        };
        let Some(value) = value else {
            continue;
        };

        // Hashing a UTF-8 constant visits its bytes. Account for that work here,
        // then retain its ID by instruction so CFG reprocessing does not repeat
        // a string lookup for each visit.
        charge(fact_work, value.len().saturating_add(1), pc)?;
        let id = if let Some(id) = ids.get(value.as_ref()).copied() {
            id
        } else {
            let id = u32::try_from(strings.len())
                .map_err(|_| failure(pc, "too many string constants for verification"))?;
            ids.try_reserve(1)
                .map_err(|_| failure(pc, "cannot allocate verifier string index"))?;
            strings
                .try_reserve(1)
                .map_err(|_| failure(pc, "cannot allocate verifier string table"))?;
            ids.insert(value.clone(), id);
            strings.push(value.clone());
            id
        };
        instruction_ids[pc] = Some(id);
    }

    Ok(InternedStrings {
        instruction_ids,
        values: strings,
    })
}

fn charge(budget: &mut usize, amount: usize, pc: usize) -> Result<(), String> {
    *budget = budget
        .checked_sub(amount)
        .ok_or_else(|| failure(pc, "verification complexity limit exceeded"))?;
    Ok(())
}

fn write_free_callables(
    program: &CompiledProgram,
    fact_work: &mut usize,
) -> Result<BTreeSet<usize>, String> {
    let mut candidates = BTreeMap::<usize, BTreeSet<usize>>::new();
    for (entry, _) in program.callables.iter().take(MAX_SUMMARIZED_CALLABLES) {
        let end = program
            .callables
            .range(entry + 1..)
            .next()
            .map_or(program.instructions.len(), |(next, _)| *next);
        let mut calls = BTreeSet::new();
        let mut writes = false;
        for (offset, instruction) in program.instructions[*entry..end].iter().enumerate() {
            charge(fact_work, 1, entry + offset)?;
            match instruction {
                Instruction::StoreGlobal(_) | Instruction::CallValue(_) => {
                    writes = true;
                    break;
                }
                Instruction::Call(target, _) => {
                    calls.insert(*target);
                    if calls.len() > MAX_SUMMARIZED_CALLABLES {
                        writes = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !writes {
            candidates.insert(*entry, calls);
        }
    }
    // Start with all syntactically write-free regions. Removing a dependency
    // also removes its callers; recursive cycles without writes are safe.
    loop {
        let mut removed = Vec::new();
        removed
            .try_reserve(candidates.len())
            .map_err(|_| failure(0, "cannot allocate callable effect summaries"))?;
        for (entry, calls) in &candidates {
            charge(fact_work, calls.len() + 1, *entry)?;
            if calls.iter().any(|target| !candidates.contains_key(target)) {
                removed.push(*entry);
            }
        }
        if removed.is_empty() {
            break;
        }
        for entry in removed {
            candidates.remove(&entry);
        }
    }
    Ok(candidates.into_keys().collect())
}

fn callable_result_summaries(
    program: &CompiledProgram,
    instruction_string_ids: &[Option<u32>],
    strings: &[Rc<str>],
    write_free_callables: &BTreeSet<usize>,
    fact_work: &mut usize,
) -> Result<BTreeMap<usize, FlowValue>, String> {
    let mut results = BTreeMap::new();
    loop {
        let context = VerifyContext {
            program,
            instruction_string_ids,
            strings,
            callable_results: &results,
            write_free_callables,
        };
        let mut next_results = BTreeMap::new();
        for (entry, callable) in program.callables.iter().take(MAX_SUMMARIZED_CALLABLES) {
            charge(fact_work, 1, *entry)?;
            let end = program
                .callables
                .range(entry + 1..)
                .next()
                .map_or(program.instructions.len(), |(next, _)| *next);
            if end - entry > MAX_SUMMARY_INSTRUCTIONS || callable.parameters > MAX_SUMMARY_STACK {
                continue;
            }
            let mut state = State {
                height: callable.parameters,
                initialized: Rc::default(),
                stack_values: Rc::default(),
                aggregate_shapes: Rc::default(),
                record_shapes: Rc::default(),
                local_values: Rc::default(),
                global_values: Rc::default(),
                capture_values: None,
            };
            for pc in *entry..end {
                charge(fact_work, 1, pc)?;
                let instruction = &program.instructions[pc];
                let (required, produced) = instruction
                    .stack_effect(&program.imports)
                    .map_err(|error| failure(pc, &error.to_string()))?;
                if state.height < required {
                    break;
                }
                match instruction {
                    Instruction::Return => {
                        let value = flow_at(&state.stack_values, state.height - 1);
                        let value = if let Some(expected) = &callable.result_type {
                            if normal_type_may_match_runtime_type(
                                value, expected, strings, pc, fact_work,
                            )? {
                                FlowValue::recoverable(
                                    value.normal_types
                                        & runtime_type_mask(expected, fact_work, pc)?,
                                )
                            } else {
                                FlowValue::ERROR
                            }
                        } else {
                            value
                        };
                        if !value.is_unknown() {
                            next_results.insert(*entry, value);
                        }
                        break;
                    }
                    // No path-sensitive or recursive argument specialization.
                    // Unsupported regions keep the ordinary dynamic summary.
                    Instruction::Jump(_)
                    | Instruction::JumpIfFalse(_)
                    | Instruction::JumpIfTrue(_)
                    | Instruction::Halt
                    | Instruction::Crash(_) => break,
                    _ => {}
                }
                let old_height = state.height;
                state.height = if let Instruction::StackTruncate(depth) = instruction {
                    *depth
                } else {
                    old_height - required + produced
                };
                if state.height > MAX_SUMMARY_STACK {
                    break;
                }
                update_value_facts(
                    &context,
                    instruction,
                    old_height,
                    required,
                    &mut state,
                    pc,
                    fact_work,
                )?;
            }
        }
        if next_results == results {
            return Ok(results);
        }
        results = next_results;
    }
}

fn region(
    context: &VerifyContext<'_>,
    start: usize,
    end: usize,
    parameters: usize,
    captures: usize,
    flow: &mut FlowAnalysis<'_>,
) -> Result<(), String> {
    let program = context.program;
    // A local store affects this frame only. Captures are supplied by the
    // checked constructor, and branches cannot enter another frame's region.
    for pc in start..end {
        charge(flow.visits, 1, pc)?;
        match program.instructions[pc] {
            Instruction::Jump(target)
            | Instruction::JumpIfFalse(target)
            | Instruction::JumpIfTrue(target)
                if !(start..end).contains(&target) =>
            {
                return Err(failure(pc, "branch crosses a callable region"));
            }
            Instruction::LoadCapture(index) if index >= captures => {
                return Err(failure(pc, "capture index exceeds callable signature"));
            }
            Instruction::Return if start == 0 => {
                return Err(failure(pc, "return outside a callable"));
            }
            _ => {}
        }
    }
    let allocation_error = || failure(start, "cannot allocate verification state");
    let mut states: Vec<Option<State>> = Vec::new();
    states
        .try_reserve_exact(end - start)
        .map_err(|_| allocation_error())?;
    states.resize_with(end - start, || None);
    let mut queued = Vec::new();
    queued
        .try_reserve_exact(end - start)
        .map_err(|_| allocation_error())?;
    queued.resize(end - start, false);
    let mut work = Vec::new();
    work.try_reserve_exact(end - start)
        .map_err(|_| allocation_error())?;
    work.push(start);
    states[0] = Some(State {
        height: parameters,
        initialized: Rc::default(),
        stack_values: Rc::default(),
        aggregate_shapes: Rc::default(),
        record_shapes: Rc::default(),
        local_values: Rc::default(),
        global_values: Rc::default(),
        capture_values: flow.capture_values.map(|values| Rc::new(values.to_vec())),
    });
    queued[0] = true;
    while let Some(pc) = work.pop() {
        charge(flow.visits, 1, pc)?;
        queued[pc - start] = false;
        let mut state = states[pc - start].as_ref().unwrap().clone();
        let instruction = &program.instructions[pc];
        let (required, produced) = instruction
            .stack_effect(&program.imports)
            .map_err(|error| failure(pc, &error.to_string()))?;
        if state.height < required {
            return Err(failure(pc, "operand stack underflow"));
        }
        if let Instruction::JumpIfFalse(_) | Instruction::JumpIfTrue(_) = instruction {
            let condition = flow_at(&state.stack_values, state.height - 1);
            if condition.normal_types & TYPE_BOOL == 0 && !condition.may_error {
                return Err(failure(pc, "branch condition cannot be a boolean"));
            }
        }
        if let Instruction::CallValue(arity) = instruction
            && let Some(target) = flow_at(&state.stack_values, state.height - 1).callable
        {
            let callable = signature(program, target.entry)?;
            if callable.parameters != *arity || callable.captures != target.captures {
                return Err(failure(pc, "indirect call signature mismatch"));
            }
        }
        let branch_condition = match instruction {
            Instruction::JumpIfFalse(_) | Instruction::JumpIfTrue(_) => {
                flow_at(&state.stack_values, state.height - 1).known_bool
            }
            _ => None,
        };
        match instruction {
            Instruction::Load(slot) if !state.initialized.contains(slot) => {
                return Err(failure(pc, "local is not initialized on every path"));
            }
            Instruction::Store(slot) if !state.initialized.contains(slot) => {
                charge(flow.fact_work, state.initialized.len() + 1, pc)?;
                Rc::make_mut(&mut state.initialized).insert(*slot);
            }
            _ => {}
        }
        let previous_height = state.height;
        state.height = if let Instruction::StackTruncate(depth) = instruction {
            *depth
        } else {
            state.height - required + produced
        };
        if state.height > LIMIT {
            return Err(failure(pc, "operand stack exceeds verification limit"));
        }
        if let Instruction::MakeClosure(entry, count) = instruction {
            record_closure_captures(
                flow.closure_outputs,
                *entry,
                *count,
                previous_height,
                &state.stack_values,
                pc,
                flow.fact_work,
            )?;
        }
        update_value_facts(
            context,
            instruction,
            previous_height,
            required,
            &mut state,
            pc,
            flow.fact_work,
        )?;
        let mut successors = [None, None];
        match instruction {
            Instruction::Return | Instruction::Halt | Instruction::Crash(_) => {}
            Instruction::Jump(target) => successors[0] = Some(*target),
            Instruction::JumpIfFalse(target) | Instruction::JumpIfTrue(target) => {
                let jump_taken = branch_condition.is_some_and(|condition| match instruction {
                    Instruction::JumpIfFalse(_) => !condition,
                    Instruction::JumpIfTrue(_) => condition,
                    _ => unreachable!(),
                });
                if branch_condition.is_some() {
                    successors[0] = Some(if jump_taken { *target } else { pc + 1 });
                } else {
                    successors = [Some(*target), Some(pc + 1)];
                }
            }
            _ => successors[0] = Some(pc + 1),
        }
        for successor in successors.into_iter().flatten() {
            if successor >= end {
                return Err(failure(pc, "execution falls outside its callable region"));
            }
            let changed = if let Some(previous) = &mut states[successor - start] {
                if previous.height != state.height {
                    return Err(failure(
                        successor,
                        "inconsistent operand stack heights at merge",
                    ));
                }
                let mut changed = false;
                if !Rc::ptr_eq(&previous.initialized, &state.initialized) {
                    charge(flow.fact_work, previous.initialized.len() + 1, pc)?;
                    let common: BTreeSet<_> = previous
                        .initialized
                        .intersection(&state.initialized)
                        .copied()
                        .collect();
                    if common.len() == previous.initialized.len() {
                        // The predecessor already knew every local initialized
                        // on this path.
                    } else {
                        previous.initialized = Rc::new(common);
                        changed = true;
                    }
                }
                for (previous_facts, incoming_facts) in [
                    (&mut previous.stack_values, &state.stack_values),
                    (&mut previous.local_values, &state.local_values),
                    (&mut previous.global_values, &state.global_values),
                ] {
                    if Rc::ptr_eq(previous_facts, incoming_facts) {
                        continue;
                    }
                    charge(
                        flow.fact_work,
                        previous_facts.len().saturating_add(incoming_facts.len()),
                        successor,
                    )?;
                    let common = join_value_maps(previous_facts, incoming_facts);
                    if common != **previous_facts {
                        *previous_facts = Rc::new(common);
                        changed = true;
                    }
                }
                if !Rc::ptr_eq(&previous.aggregate_shapes, &state.aggregate_shapes) {
                    charge(
                        flow.fact_work,
                        aggregate_shape_cost(&previous.aggregate_shapes)
                            .saturating_add(aggregate_shape_cost(&state.aggregate_shapes)),
                        successor,
                    )?;
                    let common =
                        join_aggregate_shapes(&previous.aggregate_shapes, &state.aggregate_shapes);
                    if &common != previous.aggregate_shapes.as_ref() {
                        previous.aggregate_shapes = Rc::new(common);
                        changed = true;
                    }
                }
                if !Rc::ptr_eq(&previous.record_shapes, &state.record_shapes) {
                    charge(
                        flow.fact_work,
                        record_shape_cost(&previous.record_shapes)
                            .saturating_add(record_shape_cost(&state.record_shapes)),
                        successor,
                    )?;
                    let common = join_record_shapes(&previous.record_shapes, &state.record_shapes);
                    if &common != previous.record_shapes.as_ref() {
                        previous.record_shapes = Rc::new(common);
                        changed = true;
                    }
                }
                changed
            } else {
                states[successor - start] = Some(state.clone());
                true
            };
            if changed && !queued[successor - start] {
                queued[successor - start] = true;
                work.push(successor);
            }
        }
    }
    verify_dead_stack_heights(program, start, end, &states, flow.visits)?;
    Ok(())
}

fn stack_successors(instruction: &Instruction, pc: usize) -> [Option<usize>; 2] {
    match instruction {
        Instruction::Return | Instruction::Halt | Instruction::Crash(_) => [None, None],
        Instruction::Jump(target) => [Some(*target), None],
        Instruction::JumpIfFalse(target) | Instruction::JumpIfTrue(target) => {
            [Some(*target), Some(pc + 1)]
        }
        _ => [Some(pc + 1), None],
    }
}

fn verify_dead_stack_heights(
    program: &CompiledProgram,
    start: usize,
    end: usize,
    reachable: &[Option<State>],
    visits: &mut usize,
) -> Result<(), String> {
    let allocation_error = || failure(start, "cannot allocate stack verification state");
    let mut heights = Vec::new();
    heights
        .try_reserve_exact(end - start)
        .map_err(|_| allocation_error())?;
    heights.resize(end - start, None);
    let mut has_predecessor = Vec::new();
    has_predecessor
        .try_reserve_exact(end - start)
        .map_err(|_| allocation_error())?;
    has_predecessor.resize(end - start, false);
    let mut work = Vec::new();
    work.try_reserve_exact(end - start)
        .map_err(|_| allocation_error())?;

    // A value-proven impossible branch is still structurally stack-checked,
    // using the outgoing height at that branch. It must not be merged into a
    // feasible target state, since it cannot execute at runtime.
    for pc in start..end {
        charge(visits, 1, pc)?;
        let index = pc - start;
        let Some(state) = reachable[index].as_ref() else {
            continue;
        };
        let instruction = &program.instructions[pc];
        if let Instruction::JumpIfFalse(target) | Instruction::JumpIfTrue(target) = instruction
            && let Some(condition) = flow_at(&state.stack_values, state.height - 1).known_bool
        {
            let jump_taken = match instruction {
                Instruction::JumpIfFalse(_) => !condition,
                Instruction::JumpIfTrue(_) => condition,
                _ => unreachable!(),
            };
            let impossible_successor = if jump_taken { pc + 1 } else { *target };
            if (start..end).contains(&impossible_successor)
                && reachable[impossible_successor - start].is_none()
            {
                let outgoing_height = state.height - 1;
                queue_dead_stack_state(
                    &mut heights,
                    &mut work,
                    start,
                    impossible_successor,
                    outgoing_height,
                )?;
            }
        }
    }

    // Build predecessor information only inside the value-unreachable graph.
    // Edges from statically impossible branches into live code do not alter
    // the live program's stack contract.
    for pc in start..end {
        charge(visits, 1, pc)?;
        if reachable[pc - start].is_some() {
            continue;
        }
        let successors = stack_successors(&program.instructions[pc], pc);
        for successor in successors.into_iter().flatten() {
            if !(start..end).contains(&successor) {
                return Err(failure(pc, "execution falls outside its callable region"));
            }
            if reachable[successor - start].is_none() {
                has_predecessor[successor - start] = true;
            }
        }
    }

    drain_dead_stack_work(
        program,
        start,
        end,
        reachable,
        &mut heights,
        &mut work,
        visits,
    )?;

    // Detached source blocks have no stack supplied by executable code. Start
    // them empty, then follow their full CFG so backward jumps inherit values
    // from the block's root instead of becoming separate synthetic entries.
    for pc in start..end {
        charge(visits, 1, pc)?;
        let index = pc - start;
        if reachable[index].is_none() && heights[index].is_none() && !has_predecessor[index] {
            queue_dead_stack_state(&mut heights, &mut work, start, pc, 0)?;
            drain_dead_stack_work(
                program,
                start,
                end,
                reachable,
                &mut heights,
                &mut work,
                visits,
            )?;
        }
    }
    // A detached cycle has no source node. Seed one representative and check
    // the entire cycle for underflow, inconsistent heights, and stack growth.
    for pc in start..end {
        charge(visits, 1, pc)?;
        let index = pc - start;
        if reachable[index].is_none() && heights[index].is_none() {
            queue_dead_stack_state(&mut heights, &mut work, start, pc, 0)?;
            drain_dead_stack_work(
                program,
                start,
                end,
                reachable,
                &mut heights,
                &mut work,
                visits,
            )?;
        }
    }
    Ok(())
}

fn queue_dead_stack_state(
    heights: &mut [Option<usize>],
    work: &mut Vec<usize>,
    start: usize,
    pc: usize,
    height: usize,
) -> Result<(), String> {
    match heights[pc - start] {
        Some(previous_height) if previous_height != height => {
            Err(failure(pc, "inconsistent operand stack heights at merge"))
        }
        Some(_) => Ok(()),
        None => {
            heights[pc - start] = Some(height);
            work.push(pc);
            Ok(())
        }
    }
}

fn drain_dead_stack_work(
    program: &CompiledProgram,
    start: usize,
    end: usize,
    reachable: &[Option<State>],
    heights: &mut [Option<usize>],
    work: &mut Vec<usize>,
    visits: &mut usize,
) -> Result<(), String> {
    while let Some(pc) = work.pop() {
        charge(visits, 1, pc)?;
        let height = heights[pc - start].expect("queued stack state has a height");
        let instruction = &program.instructions[pc];
        let (required, produced) = instruction
            .stack_effect(&program.imports)
            .map_err(|error| failure(pc, &error.to_string()))?;
        if height < required {
            return Err(failure(pc, "operand stack underflow"));
        }
        let next_height = if let Instruction::StackTruncate(depth) = instruction {
            *depth
        } else {
            height
                .checked_sub(required)
                .and_then(|height| height.checked_add(produced))
                .ok_or_else(|| failure(pc, "operand stack height overflow"))?
        };
        if next_height > LIMIT {
            return Err(failure(pc, "operand stack exceeds verification limit"));
        }

        let successors = stack_successors(instruction, pc);
        for successor in successors.into_iter().flatten() {
            if !(start..end).contains(&successor) {
                return Err(failure(pc, "execution falls outside its callable region"));
            }
            let successor_index = successor - start;
            if reachable[successor_index].is_some() {
                continue;
            }
            if let Some(previous_height) = heights[successor_index] {
                if previous_height != next_height {
                    return Err(failure(
                        successor,
                        "inconsistent operand stack heights at merge",
                    ));
                }
            } else {
                queue_dead_stack_state(heights, work, start, successor, next_height)?;
            }
        }
    }
    Ok(())
}

fn record_closure_captures(
    summaries: &mut BTreeMap<usize, Vec<FlowValue>>,
    entry: usize,
    count: usize,
    old_height: usize,
    stack_values: &BTreeMap<usize, FlowValue>,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    // Missing or oversized summaries mean fully dynamic capture values.
    if count == 0
        || count > MAX_TRACKED_AGGREGATE_ITEMS
        || (!summaries.contains_key(&entry) && summaries.len() >= MAX_TRACKED_CLOSURE_TARGETS)
    {
        return Ok(());
    }
    charge(fact_work, count.saturating_add(1), pc)?;
    let first_capture = old_height - count;
    let incoming: Vec<_> = (first_capture..old_height)
        .map(|slot| flow_at(stack_values, slot))
        .collect();
    let summary = summaries.entry(entry).or_insert_with(|| incoming.clone());
    for (current, value) in summary.iter_mut().zip(incoming) {
        *current = current.join(value);
    }
    Ok(())
}

fn flow_at(facts: &BTreeMap<usize, FlowValue>, slot: usize) -> FlowValue {
    facts.get(&slot).copied().unwrap_or(FlowValue::UNKNOWN)
}

fn aggregate_index_flow(state: &State, old_height: usize) -> FlowValue {
    let aggregate_slot = old_height - 2;
    let key = flow_at(&state.stack_values, old_height - 1);
    if let Some(index) = key.known_int {
        return if let Some(shape) = state.aggregate_shapes.get(&aggregate_slot) {
            usize::try_from(index)
                .ok()
                .and_then(|index| shape.get(index).copied())
                .unwrap_or(FlowValue::ERROR)
        } else if state.record_shapes.contains_key(&aggregate_slot) {
            FlowValue::ERROR
        } else {
            FlowValue::UNKNOWN
        };
    }
    if let Some(field) = key.known_string {
        return if let Some(shape) = state.record_shapes.get(&aggregate_slot) {
            shape.get(&field).copied().unwrap_or(FlowValue::ERROR)
        } else if state.aggregate_shapes.contains_key(&aggregate_slot) {
            FlowValue::ERROR
        } else {
            FlowValue::UNKNOWN
        };
    }
    FlowValue::UNKNOWN
}

fn value_flow(value: &Value, string_id: Option<u32>) -> FlowValue {
    match value {
        Value::Int(value) => FlowValue::integer(*value),
        Value::Float(_) => FlowValue::normal(TYPE_FLOAT),
        Value::Bool(value) => FlowValue::boolean(*value),
        Value::Nil => FlowValue::normal(TYPE_NIL),
        Value::Error(_) => FlowValue::ERROR,
        Value::Str(_) => string_id
            .map(FlowValue::string)
            .unwrap_or_else(|| FlowValue::normal(TYPE_STR)),
        Value::StrBuf(_) => FlowValue::normal(TYPE_STRBUF),
        // A serialized in-memory array value can also carry tuple metadata.
        // Do not inspect its RefCell-backed descriptor during verification.
        Value::Array(_) => FlowValue::normal(TYPE_ARRAY | TYPE_TUPLE),
        Value::Record(_) => FlowValue::normal(TYPE_RECORD),
        Value::Variant(_, _) => {
            let mut flow = FlowValue::normal(TYPE_VARIANT);
            flow.known_tag = string_id;
            flow
        }
        Value::Function(entry) => FlowValue {
            normal_types: TYPE_FUNCTION,
            may_error: false,
            callable: Some(CallableTarget {
                entry: *entry,
                captures: 0,
            }),
            is_type_tag: false,
            known_tag: None,
            known_bool: None,
            known_int: None,
            known_string: None,
        },
        Value::Closure(_, _) => FlowValue::normal(TYPE_FUNCTION),
        Value::TypeTag(_) => {
            let mut flow = FlowValue::type_tag();
            flow.known_tag = string_id;
            flow
        }
        Value::Bytes(_) | Value::Handle(_) | Value::Cell(_) | Value::Iterator(_) => {
            FlowValue::normal(TYPE_OTHER)
        }
    }
}

fn runtime_type_mask(
    ty: &crate::value::RuntimeType,
    fact_work: &mut usize,
    pc: usize,
) -> Result<u16, String> {
    use crate::value::RuntimeType;
    charge(fact_work, 1, pc)?;
    Ok(match ty {
        RuntimeType::Int => TYPE_INT,
        RuntimeType::Float => TYPE_FLOAT,
        RuntimeType::Bool => TYPE_BOOL,
        RuntimeType::Nil => TYPE_NIL,
        // Runtime `str` contracts also accept the internal mutable string
        // builder, while `IsType(str)` intentionally matches only Value::Str.
        RuntimeType::Str => TYPE_STR | TYPE_STRBUF,
        RuntimeType::Error(_) => 0,
        RuntimeType::Array(_) => TYPE_ARRAY,
        RuntimeType::Tuple(_) => TYPE_TUPLE,
        RuntimeType::Record => TYPE_RECORD,
        RuntimeType::Variant(_) => TYPE_VARIANT | TYPE_TYPE_TAG,
        RuntimeType::Function => TYPE_FUNCTION,
        RuntimeType::Other => TYPE_OTHER,
        RuntimeType::Nominal(_) => TYPE_RECORD | TYPE_VARIANT | TYPE_TYPE_TAG,
        RuntimeType::Nullable(inner) => runtime_type_mask(inner, fact_work, pc)? | TYPE_NIL,
    })
}

fn callable_result_flow(
    context: &VerifyContext<'_>,
    target: usize,
    arguments: impl Iterator<Item = FlowValue>,
    pc: usize,
    fact_work: &mut usize,
) -> Result<FlowValue, String> {
    let callable = signature(context.program, target)?;
    let compatibility =
        callable_argument_compatibility(callable, arguments, context.strings, pc, fact_work)?;
    if compatibility.definite_mismatch {
        return Ok(FlowValue::ERROR);
    }
    let result = match context.callable_results.get(&target).copied() {
        Some(result) => result,
        None => match callable.result_type.as_ref() {
            Some(ty) => FlowValue::recoverable(runtime_type_mask(ty, fact_work, pc)?),
            None => FlowValue::UNKNOWN,
        },
    };
    Ok(if compatibility.may_mismatch {
        result.join(FlowValue::ERROR)
    } else {
        result
    })
}

fn normal_type_may_match_runtime_type(
    value: FlowValue,
    expected: &crate::value::RuntimeType,
    strings: &[Rc<str>],
    pc: usize,
    fact_work: &mut usize,
) -> Result<bool, String> {
    use crate::value::RuntimeType;

    charge(fact_work, 1, pc)?;
    let tag = value
        .known_tag
        .and_then(|tag| strings.get(tag as usize))
        .map(|tag| tag.as_ref());
    if let RuntimeType::Nominal(name) | RuntimeType::Variant(name) = expected {
        charge(
            fact_work,
            name.len().saturating_add(tag.map_or(0, str::len)),
            pc,
        )?;
    }
    Ok(match expected {
        RuntimeType::Int => value.normal_types & TYPE_INT != 0,
        RuntimeType::Float => value.normal_types & TYPE_FLOAT != 0,
        RuntimeType::Bool => value.normal_types & TYPE_BOOL != 0,
        RuntimeType::Nil => value.normal_types & TYPE_NIL != 0,
        RuntimeType::Str => value.normal_types & (TYPE_STR | TYPE_STRBUF) != 0,
        RuntimeType::Error(_) => false,
        RuntimeType::Array(_) => value.normal_types & TYPE_ARRAY != 0,
        RuntimeType::Record => {
            value.normal_types & TYPE_RECORD != 0
                && !tag.is_some_and(|tag| tag.starts_with("@struct::"))
        }
        RuntimeType::Variant(name) => {
            (value.normal_types & TYPE_TYPE_TAG != 0 && tag.is_none_or(|tag| tag == name))
                || (value.normal_types & TYPE_VARIANT != 0
                    && tag.is_none_or(|tag| {
                        tag.rsplit_once("::").map_or(tag, |(parent, _)| parent) == name
                    }))
        }
        RuntimeType::Function => value.normal_types & TYPE_FUNCTION != 0,
        RuntimeType::Other => value.normal_types & TYPE_OTHER != 0,
        RuntimeType::Nominal(name) => {
            let record_matches = value.normal_types & TYPE_RECORD != 0
                && tag.is_none_or(|tag| {
                    tag.strip_prefix("@struct::")
                        .is_none_or(|record| record == name)
                });
            // RuntimeType::of preserves a metadata tag verbatim, but strips
            // the final component of an actual variant's tag.
            let metadata_matches =
                value.normal_types & TYPE_TYPE_TAG != 0 && tag.is_none_or(|tag| tag == name);
            let variant_matches = value.normal_types & TYPE_VARIANT != 0
                && tag.is_none_or(|tag| {
                    tag.rsplit_once("::").map_or(tag, |(parent, _)| parent) == name
                });
            record_matches || metadata_matches || variant_matches
        }
        RuntimeType::Nullable(inner) => {
            value.normal_types & TYPE_NIL != 0
                || normal_type_may_match_runtime_type(value, inner, strings, pc, fact_work)?
        }
        // The current flow domain tracks whether a tuple exists, but does not
        // carry its complete runtime descriptor through calls and aliases.
        RuntimeType::Tuple(_) => value.normal_types & TYPE_TUPLE != 0,
    })
}

fn normal_type_definitely_matches_runtime_type(
    value: FlowValue,
    expected: &crate::value::RuntimeType,
    strings: &[Rc<str>],
    pc: usize,
    fact_work: &mut usize,
) -> Result<bool, String> {
    use crate::value::RuntimeType;

    charge(fact_work, 1, pc)?;
    if value.normal_types == 0 {
        return Ok(true);
    }
    let tag = value
        .known_tag
        .and_then(|tag| strings.get(tag as usize))
        .map(|tag| tag.as_ref());
    if let RuntimeType::Nominal(name) | RuntimeType::Variant(name) = expected {
        charge(
            fact_work,
            name.len().saturating_add(tag.map_or(0, str::len)),
            pc,
        )?;
    }
    Ok(match expected {
        RuntimeType::Int => value.normal_types == TYPE_INT,
        RuntimeType::Float => value.normal_types == TYPE_FLOAT,
        RuntimeType::Bool => value.normal_types == TYPE_BOOL,
        RuntimeType::Nil => value.normal_types == TYPE_NIL,
        RuntimeType::Str => value.normal_types & !(TYPE_STR | TYPE_STRBUF) == 0,
        RuntimeType::Error(_) => false,
        // FlowValue intentionally does not retain the complete generic
        // descriptor of arrays, tuples or records. Their runtime contracts
        // may therefore still fail even when the outer category matches.
        RuntimeType::Array(_) | RuntimeType::Tuple(_) | RuntimeType::Record => false,
        RuntimeType::Variant(name) => match value.normal_types {
            TYPE_TYPE_TAG => tag.is_some_and(|tag| tag == name),
            TYPE_VARIANT => tag
                .is_some_and(|tag| tag.rsplit_once("::").map_or(tag, |(parent, _)| parent) == name),
            _ => false,
        },
        RuntimeType::Function => value.normal_types == TYPE_FUNCTION,
        RuntimeType::Other => value.normal_types == TYPE_OTHER,
        RuntimeType::Nominal(name) => match value.normal_types {
            TYPE_RECORD => {
                tag.is_some_and(|tag| tag.strip_prefix("@struct::") == Some(name.as_str()))
            }
            TYPE_TYPE_TAG => tag.is_some_and(|tag| tag == name),
            TYPE_VARIANT => tag
                .is_some_and(|tag| tag.rsplit_once("::").map_or(tag, |(parent, _)| parent) == name),
            _ => false,
        },
        RuntimeType::Nullable(inner) => {
            let non_nil = value.normal_types & !TYPE_NIL;
            non_nil == 0
                || normal_type_definitely_matches_runtime_type(
                    FlowValue {
                        normal_types: non_nil,
                        ..value
                    },
                    inner,
                    strings,
                    pc,
                    fact_work,
                )?
        }
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CallableArgumentCompatibility {
    definite_mismatch: bool,
    may_mismatch: bool,
}

fn callable_argument_compatibility(
    callable: &CallableSignature,
    arguments: impl Iterator<Item = FlowValue>,
    strings: &[Rc<str>],
    pc: usize,
    fact_work: &mut usize,
) -> Result<CallableArgumentCompatibility, String> {
    let Some(parameter_types) = &callable.parameter_types else {
        return Ok(CallableArgumentCompatibility::default());
    };
    let mut compatibility = CallableArgumentCompatibility::default();
    for (expected, actual) in parameter_types.iter().zip(arguments) {
        charge(fact_work, 1, pc)?;
        // A Cog Error is accepted at a typed call boundary and may be
        // handled by the callee. Do not turn that path into a
        // statically known call failure.
        if let Some(expected) = expected {
            if actual.normal_types == 0 {
                continue;
            }
            let may_match =
                normal_type_may_match_runtime_type(actual, expected, strings, pc, fact_work)?;
            if !may_match && !actual.may_error {
                return Ok(CallableArgumentCompatibility {
                    definite_mismatch: true,
                    may_mismatch: false,
                });
            }
            if !normal_type_definitely_matches_runtime_type(
                actual, expected, strings, pc, fact_work,
            )? {
                compatibility.may_mismatch = true;
            }
        }
    }
    Ok(compatibility)
}

fn normal_type_may_match_native(value: FlowValue, expected: crate::runtime::NativeType) -> bool {
    use crate::runtime::NativeType;

    let mask = match expected {
        NativeType::Any => return true,
        NativeType::Int => TYPE_INT,
        NativeType::Str => TYPE_STR,
        NativeType::Bytes | NativeType::Handle => TYPE_OTHER,
        NativeType::Nil => TYPE_NIL,
        NativeType::Number => TYPE_INT | TYPE_FLOAT,
        NativeType::Array => TYPE_ARRAY | TYPE_TUPLE,
        NativeType::Record => TYPE_RECORD,
        NativeType::Float => TYPE_FLOAT,
        NativeType::Bool => TYPE_BOOL,
        NativeType::StrBuf => TYPE_STRBUF,
    };
    value.normal_types & mask != 0
}

fn native_arguments_definitely_mismatch(
    import: &crate::runtime::NativeImport,
    arguments: impl Iterator<Item = FlowValue>,
    pc: usize,
    fact_work: &mut usize,
) -> Result<bool, String> {
    for (expected, actual) in import.parameters().iter().copied().zip(arguments) {
        charge(fact_work, 1, pc)?;
        // Typed native parameters propagate Error without entering host code.
        // Any deliberately accepts Error and may return a normal value.
        if !normal_type_may_match_native(actual, expected) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn native_result_flow(
    import: &crate::runtime::NativeImport,
    first_argument: FlowValue,
) -> FlowValue {
    use crate::runtime::{NativeResultContract, NativeType};

    let categories = match import.result_contract() {
        NativeResultContract::Declared => None,
        NativeResultContract::OptionalString => Some(TYPE_STR | TYPE_NIL),
        NativeResultContract::Json => {
            Some(TYPE_NIL | TYPE_BOOL | TYPE_INT | TYPE_FLOAT | TYPE_STR | TYPE_ARRAY | TYPE_RECORD)
        }
        NativeResultContract::Iterator => Some(TYPE_OTHER),
        NativeResultContract::Error => return FlowValue::ERROR,
        NativeResultContract::StructuralClone => Some(
            first_argument.normal_types
                & (TYPE_INT
                    | TYPE_FLOAT
                    | TYPE_BOOL
                    | TYPE_NIL
                    | TYPE_STR
                    | TYPE_ARRAY
                    | TYPE_TUPLE),
        ),
    };
    if let Some(categories) = categories {
        return FlowValue::recoverable(categories);
    }

    let mask = match import.result() {
        NativeType::Any => return FlowValue::UNKNOWN,
        NativeType::Int => TYPE_INT,
        NativeType::Float => TYPE_FLOAT,
        NativeType::Bool => TYPE_BOOL,
        NativeType::Str => TYPE_STR,
        NativeType::StrBuf => TYPE_STRBUF,
        NativeType::Nil => TYPE_NIL,
        NativeType::Number => TYPE_INT | TYPE_FLOAT,
        NativeType::Array => TYPE_ARRAY | TYPE_TUPLE,
        NativeType::Record => TYPE_RECORD,
        NativeType::Bytes | NativeType::Handle => TYPE_OTHER,
    };
    FlowValue::recoverable(mask)
}

fn primitive_type_test(value: FlowValue, expected: u16) -> Option<bool> {
    if value.normal_types & expected == 0 {
        // Error values do not match primitive type predicates.
        Some(false)
    } else if value.normal_types == expected && !value.may_error {
        Some(true)
    } else {
        None
    }
}

fn type_test_flow(
    expected: &crate::instruction::ValueType,
    value: FlowValue,
    strings: &[Rc<str>],
    pc: usize,
    fact_work: &mut usize,
) -> Result<FlowValue, String> {
    use crate::instruction::ValueType;

    let tag = (!value.is_type_tag)
        .then_some(value.known_tag)
        .flatten()
        .and_then(|tag| strings.get(tag as usize))
        .map(|tag| tag.as_ref());
    if let ValueType::Enum(name) | ValueType::Variant(name) = expected {
        charge(
            fact_work,
            name.len().saturating_add(tag.map_or(0, str::len)),
            pc,
        )?;
    }
    let known = match expected {
        ValueType::Int => primitive_type_test(value, TYPE_INT),
        ValueType::Float => primitive_type_test(value, TYPE_FLOAT),
        ValueType::Bool => primitive_type_test(value, TYPE_BOOL),
        ValueType::Str => primitive_type_test(value, TYPE_STR),
        ValueType::Nil => primitive_type_test(value, TYPE_NIL),
        ValueType::Error if value.normal_types == 0 && value.may_error => Some(true),
        ValueType::Error if !value.may_error => Some(false),
        ValueType::Error => None,
        // TypeTag values are metadata operands, not enum values. For a
        // constructed variant, the exact immutable tag can prove its enum.
        ValueType::Enum(_) if value.is_type_tag => Some(false),
        ValueType::Enum(_) if value.normal_types & TYPE_VARIANT == 0 => Some(false),
        ValueType::Enum(name)
            if value.normal_types == TYPE_VARIANT
                && !value.may_error
                && let Some(tag) = tag =>
        {
            Some(
                tag.rsplit_once("::")
                    .is_some_and(|(parent, _)| parent == name.as_ref()),
            )
        }
        ValueType::Enum(_) => None,
        // Exact tagged values and language Errors can both be matched. Other
        // possible Error values keep their runtime predicate behavior.
        ValueType::Variant(_) if value.is_type_tag => Some(false),
        ValueType::Variant(_) if value.normal_types & TYPE_VARIANT == 0 && !value.may_error => {
            Some(false)
        }
        ValueType::Variant(name)
            if let Some(tag) = tag
                && (value.normal_types == TYPE_VARIANT
                    || (value.normal_types == 0
                        && value.may_error
                        && tag
                            .strip_prefix(crate::error::ERROR_TYPE)
                            .is_some_and(|tail| tail.starts_with("::")))) =>
        {
            Some(tag == name.as_ref())
        }
        ValueType::Variant(_) => None,
    };
    Ok(known.map_or_else(|| FlowValue::normal(TYPE_BOOL), FlowValue::boolean))
}

fn record_constructor_flow(
    context: &VerifyContext<'_>,
    count: usize,
    state: &State,
    old_height: usize,
) -> FlowValue {
    let Some(operand_count) = count.checked_mul(2) else {
        return FlowValue::UNKNOWN;
    };
    let Some(first) = old_height.checked_sub(operand_count) else {
        return FlowValue::UNKNOWN;
    };
    let mut tag_seen = false;
    let mut tag = None;
    for pair in 0..count {
        let key = flow_at(&state.stack_values, first + pair * 2);
        if key.is_type_tag {
            if tag_seen {
                // The interpreter turns every tag after the first into a
                // recoverable record-construction Error.
                return FlowValue::ERROR;
            }
            tag_seen = true;
            tag = key.known_tag;
        } else if key.known_string.is_none() {
            return FlowValue::recoverable(TYPE_RECORD | TYPE_VARIANT);
        }
    }

    if !tag_seen {
        return FlowValue::normal(TYPE_RECORD);
    }
    let Some(tag) = tag else {
        return FlowValue::recoverable(TYPE_RECORD | TYPE_VARIANT);
    };
    let Some(name) = context.strings.get(tag as usize) else {
        return FlowValue::recoverable(TYPE_RECORD | TYPE_VARIANT);
    };
    if name.starts_with("@struct::") {
        let mut flow = FlowValue::normal(TYPE_RECORD);
        flow.known_tag = Some(tag);
        flow
    } else if name
        .strip_prefix(crate::error::ERROR_TYPE)
        .is_some_and(|tail| tail.starts_with("::"))
    {
        let mut flow = FlowValue::ERROR;
        flow.known_tag = Some(tag);
        flow
    } else {
        let mut flow = FlowValue::normal(TYPE_VARIANT);
        flow.known_tag = Some(tag);
        flow
    }
}

fn produced_flow(
    context: &VerifyContext<'_>,
    instruction: &Instruction,
    state: &State,
    old_height: usize,
    pc: usize,
    fact_work: &mut usize,
) -> Result<Option<FlowValue>, String> {
    let program = context.program;
    let recoverable = FlowValue::recoverable;
    Ok(match instruction {
        Instruction::Push(value) => Some(value_flow(
            value,
            context.instruction_string_ids.get(pc).copied().flatten(),
        )),
        Instruction::PushArray(template) => Some(match template {
            ArrayTemplate::Scalar(value) => value_flow(value, None),
            // Instantiation may produce Error for incompatible element types.
            ArrayTemplate::Array(_) => recoverable(TYPE_ARRAY),
        }),
        Instruction::Load(slot) => Some(flow_at(&state.local_values, *slot)),
        Instruction::LoadGlobal(slot) => Some(flow_at(&state.global_values, *slot)),
        Instruction::LoadCapture(index) => Some(
            state
                .capture_values
                .as_deref()
                .and_then(|values| values.get(*index))
                .copied()
                .unwrap_or(FlowValue::UNKNOWN),
        ),
        Instruction::Call(target, arity) => Some(callable_result_flow(
            context,
            *target,
            (old_height - arity..old_height).map(|slot| flow_at(&state.stack_values, slot)),
            pc,
            fact_work,
        )?),
        Instruction::CallValue(arity) => {
            let callee = flow_at(&state.stack_values, old_height - 1);
            if let Some(target) = callee.callable {
                Some(callable_result_flow(
                    context,
                    target.entry,
                    (old_height - arity - 1..old_height - 1)
                        .map(|slot| flow_at(&state.stack_values, slot)),
                    pc,
                    fact_work,
                )?)
            } else {
                Some(FlowValue::UNKNOWN)
            }
        }
        Instruction::CallNative(index) => Some(if let Some(import) = program.imports.get(*index) {
            let first = old_height - import.parameters().len();
            if native_arguments_definitely_mismatch(
                import,
                (first..old_height).map(|slot| flow_at(&state.stack_values, slot)),
                pc,
                fact_work,
            )? {
                FlowValue::ERROR
            } else {
                native_result_flow(import, flow_at(&state.stack_values, first))
            }
        } else {
            FlowValue::UNKNOWN
        }),
        Instruction::Eq
        | Instruction::Ne
        | Instruction::Lt
        | Instruction::Le
        | Instruction::Gt
        | Instruction::Ge
        | Instruction::And
        | Instruction::Or
        | Instruction::Xor => {
            let left = flow_at(&state.stack_values, old_height - 2).known_bool;
            let right = flow_at(&state.stack_values, old_height - 1).known_bool;
            let value = match (instruction, left, right) {
                (Instruction::Eq, Some(left), Some(right)) => Some(left == right),
                (Instruction::Ne, Some(left), Some(right)) => Some(left != right),
                (Instruction::And, Some(left), Some(right)) => Some(left && right),
                (Instruction::Or, Some(left), Some(right)) => Some(left || right),
                (Instruction::Xor, Some(left), Some(right)) => Some(left != right),
                _ => None,
            };
            Some(value.map_or_else(|| recoverable(TYPE_BOOL), FlowValue::boolean))
        }
        Instruction::Not => Some(
            flow_at(&state.stack_values, old_height - 1)
                .known_bool
                .map_or_else(
                    || recoverable(TYPE_BOOL),
                    |value| FlowValue::boolean(!value),
                ),
        ),
        Instruction::IsType(expected) => Some(type_test_flow(
            expected,
            flow_at(&state.stack_values, old_height - 1),
            context.strings,
            pc,
            fact_work,
        )?),
        Instruction::ErrorKind => Some(FlowValue::normal(TYPE_STR)),
        Instruction::Len => Some(recoverable(TYPE_INT)),
        Instruction::Exclusive => Some(recoverable(TYPE_BOOL)),
        Instruction::MakeCell => Some(FlowValue::normal(TYPE_OTHER)),
        Instruction::CellGet => Some(FlowValue::UNKNOWN),
        Instruction::Index => Some(aggregate_index_flow(state, old_height)),
        Instruction::CellSet => Some(recoverable(TYPE_OTHER)),
        // IndexSet returns the mutated container, including tuple-backed arrays.
        Instruction::IndexSet => Some(recoverable(TYPE_ARRAY | TYPE_TUPLE | TYPE_RECORD)),
        Instruction::MakeRecord(count) => {
            // `record_constructor_flow` inspects each key to determine whether
            // the result is a record, variant, or recoverable Error. Charge
            // that scan before doing it so CFG reprocessing cannot multiply
            // an otherwise unaccounted O(count) walk.
            charge(fact_work, *count, pc)?;
            Some(record_constructor_flow(context, *count, state, old_height))
        }
        Instruction::MakeArray(_) | Instruction::MakeTypedArray(_, _) => {
            Some(recoverable(TYPE_ARRAY))
        }
        Instruction::MakeTuple(_) => Some(FlowValue::normal(TYPE_TUPLE)),
        Instruction::ConstrainArrayType(_) | Instruction::ArrayEmptyLike => {
            Some(recoverable(TYPE_ARRAY))
        }
        Instruction::ConstrainArrayFrom => Some(FlowValue::UNKNOWN),
        Instruction::MakeClosure(entry, captures) => Some(FlowValue {
            normal_types: TYPE_FUNCTION,
            may_error: false,
            callable: Some(CallableTarget {
                entry: *entry,
                captures: *captures,
            }),
            is_type_tag: false,
            known_tag: None,
            known_bool: None,
            known_int: None,
            known_string: None,
        }),
        Instruction::ArrayPush | Instruction::ArrayPrepend => Some(recoverable(TYPE_ARRAY)),
        Instruction::Add
        | Instruction::Sub
        | Instruction::Mul
        | Instruction::Div
        | Instruction::Mod => Some(FlowValue::UNKNOWN),
        Instruction::Dup
        | Instruction::Pop
        | Instruction::Swap
        | Instruction::StackTruncate(_)
        | Instruction::Store(_)
        | Instruction::StoreGlobal(_)
        | Instruction::Jump(_)
        | Instruction::JumpIfFalse(_)
        | Instruction::JumpIfTrue(_)
        | Instruction::Halt
        | Instruction::Return
        | Instruction::Crash(_) => None,
    })
}

fn update_value_facts(
    context: &VerifyContext<'_>,
    instruction: &Instruction,
    old_height: usize,
    required: usize,
    state: &mut State,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    let preserves_globals = match instruction {
        Instruction::Call(target, _) => context.write_free_callables.contains(target),
        Instruction::CallValue(_) => flow_at(&state.stack_values, old_height - 1)
            .callable
            .is_some_and(|target| context.write_free_callables.contains(&target.entry)),
        // Native callbacks cannot reassign the interpreter's global slots.
        _ => true,
    };
    let output = produced_flow(context, instruction, state, old_height, pc, fact_work)?;
    let aggregate_output = match instruction {
        Instruction::MakeTuple(count)
            if *count <= MAX_TRACKED_AGGREGATE_ITEMS
                && state.aggregate_shapes.len() + state.record_shapes.len()
                    < MAX_TRACKED_AGGREGATES =>
        {
            charge(fact_work, *count + 1, pc)?;
            let first = old_height - *count;
            Some(Rc::new(
                (first..old_height)
                    .map(|slot| flow_at(&state.stack_values, slot))
                    .collect(),
            ))
        }
        Instruction::MakeArray(count)
            if *count <= MAX_TRACKED_AGGREGATE_ITEMS
                && state.aggregate_shapes.len() + state.record_shapes.len()
                    < MAX_TRACKED_AGGREGATES =>
        {
            charge(fact_work, *count + 1, pc)?;
            let first = old_height - *count;
            let normal_types = (first..old_height).fold(0, |types, slot| {
                types | flow_at(&state.stack_values, slot).normal_types
            });
            let element = if normal_types == 0 {
                FlowValue::ERROR
            } else {
                // The union overapproximates every array that can be created;
                // Error covers constructor failure and Error-valued elements.
                FlowValue::recoverable(normal_types)
            };
            Some(Rc::new(vec![element; *count]))
        }
        Instruction::MakeTypedArray(count, expected)
            if *count <= MAX_TRACKED_AGGREGATE_ITEMS
                && state.aggregate_shapes.len() + state.record_shapes.len()
                    < MAX_TRACKED_AGGREGATES =>
        {
            charge(fact_work, *count + 1, pc)?;
            let mask = runtime_type_mask(expected, fact_work, pc)?;
            let element = if mask == 0 {
                FlowValue::ERROR
            } else {
                FlowValue::recoverable(mask)
            };
            Some(Rc::new(vec![element; *count]))
        }
        _ => None,
    };
    let record_output = match instruction {
        Instruction::MakeRecord(count)
            if *count <= MAX_TRACKED_AGGREGATE_ITEMS
                && state.aggregate_shapes.len() + state.record_shapes.len()
                    < MAX_TRACKED_AGGREGATES =>
        {
            charge(fact_work, *count * 2 + 1, pc)?;
            let first = old_height - required;
            let mut fields = BTreeMap::new();
            let mut complete = true;
            let mut type_tag_seen = false;
            // TypeTag keys carry nominal/variant metadata and are not part of
            // the runtime field map. Error-valued field values are stored, not
            // propagated.
            for pair in 0..*count {
                let key_slot = first + pair * 2;
                let key = flow_at(&state.stack_values, key_slot);
                if key.is_type_tag {
                    if type_tag_seen {
                        // The runtime accepts only the first tag. A later tag
                        // makes construction fail, so its field shape is not
                        // safe to use.
                        complete = false;
                        break;
                    }
                    type_tag_seen = true;
                    continue;
                }
                let Some(field) = key.known_string else {
                    complete = false;
                    break;
                };
                fields.insert(field, flow_at(&state.stack_values, key_slot + 1));
            }
            complete.then(|| Rc::new(fields))
        }
        _ => None,
    };
    match instruction {
        Instruction::Store(slot) => {
            let value_index = old_height - 1;
            let value = flow_at(&state.stack_values, value_index);
            set_flow(&mut state.local_values, *slot, Some(value), pc, fact_work)?;
            truncate_values(&mut state.stack_values, value_index, pc, fact_work)?;
            truncate_aggregate_shapes(&mut state.aggregate_shapes, value_index, pc, fact_work)?;
            truncate_record_shapes(&mut state.record_shapes, value_index, pc, fact_work)?;
        }
        Instruction::StoreGlobal(slot) => {
            let value_index = old_height - 1;
            let value = flow_at(&state.stack_values, value_index);
            set_flow(&mut state.global_values, *slot, Some(value), pc, fact_work)?;
            truncate_values(&mut state.stack_values, value_index, pc, fact_work)?;
            truncate_aggregate_shapes(&mut state.aggregate_shapes, value_index, pc, fact_work)?;
            truncate_record_shapes(&mut state.record_shapes, value_index, pc, fact_work)?;
        }
        Instruction::Dup => {
            let value = flow_at(&state.stack_values, old_height - 1);
            set_flow(
                &mut state.stack_values,
                old_height,
                Some(value),
                pc,
                fact_work,
            )?;
            // Dup aliases a RefCell-backed aggregate. Forget both shapes so a
            // later mutation cannot leave stale element facts.
            set_aggregate_shape(
                &mut state.aggregate_shapes,
                old_height - 1,
                None,
                pc,
                fact_work,
            )?;
            set_aggregate_shape(&mut state.aggregate_shapes, old_height, None, pc, fact_work)?;
            set_record_shape(
                &mut state.record_shapes,
                old_height - 1,
                None,
                pc,
                fact_work,
            )?;
            set_record_shape(&mut state.record_shapes, old_height, None, pc, fact_work)?;
        }
        Instruction::Swap => {
            let left = old_height - 2;
            let right = old_height - 1;
            let left_value = flow_at(&state.stack_values, left);
            let right_value = flow_at(&state.stack_values, right);
            set_flow(
                &mut state.stack_values,
                left,
                Some(right_value),
                pc,
                fact_work,
            )?;
            set_flow(
                &mut state.stack_values,
                right,
                Some(left_value),
                pc,
                fact_work,
            )?;
            let left_shape = state.aggregate_shapes.get(&left).cloned();
            let right_shape = state.aggregate_shapes.get(&right).cloned();
            set_aggregate_shape(
                &mut state.aggregate_shapes,
                left,
                right_shape,
                pc,
                fact_work,
            )?;
            set_aggregate_shape(
                &mut state.aggregate_shapes,
                right,
                left_shape,
                pc,
                fact_work,
            )?;
            let left_shape = state.record_shapes.get(&left).cloned();
            let right_shape = state.record_shapes.get(&right).cloned();
            set_record_shape(&mut state.record_shapes, left, right_shape, pc, fact_work)?;
            set_record_shape(&mut state.record_shapes, right, left_shape, pc, fact_work)?;
        }
        Instruction::StackTruncate(depth) => {
            truncate_values(&mut state.stack_values, *depth, pc, fact_work)?;
            truncate_aggregate_shapes(&mut state.aggregate_shapes, *depth, pc, fact_work)?;
            truncate_record_shapes(&mut state.record_shapes, *depth, pc, fact_work)?;
        }
        _ => {
            let first_operand = old_height - required;
            truncate_values(&mut state.stack_values, first_operand, pc, fact_work)?;
            truncate_aggregate_shapes(&mut state.aggregate_shapes, first_operand, pc, fact_work)?;
            truncate_record_shapes(&mut state.record_shapes, first_operand, pc, fact_work)?;
            if state.height > first_operand
                && let Some(value) = output
            {
                set_flow(
                    &mut state.stack_values,
                    first_operand,
                    Some(value),
                    pc,
                    fact_work,
                )?;
            }
            if state.height > first_operand
                && let Some(shape) = aggregate_output
            {
                set_aggregate_shape(
                    &mut state.aggregate_shapes,
                    first_operand,
                    Some(shape),
                    pc,
                    fact_work,
                )?;
            }
            if state.height > first_operand
                && let Some(shape) = record_output
            {
                set_record_shape(
                    &mut state.record_shapes,
                    first_operand,
                    Some(shape),
                    pc,
                    fact_work,
                )?;
            }
        }
    }
    if !preserves_globals {
        // Unknown callees and regions that may write globals retain the
        // conservative invalidation policy.
        truncate_values(&mut state.global_values, 0, pc, fact_work)?;
    }
    Ok(())
}

fn set_flow(
    facts: &mut Rc<BTreeMap<usize, FlowValue>>,
    slot: usize,
    value: Option<FlowValue>,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    if facts.get(&slot).copied().unwrap_or(FlowValue::UNKNOWN)
        == value.unwrap_or(FlowValue::UNKNOWN)
    {
        return Ok(());
    }
    charge(fact_work, facts.len().saturating_add(1), pc)?;
    let facts = Rc::make_mut(facts);
    if let Some(value) = value.filter(|value| !value.is_unknown()) {
        facts.insert(slot, value);
    } else {
        facts.remove(&slot);
    }
    Ok(())
}

fn truncate_values(
    facts: &mut Rc<BTreeMap<usize, FlowValue>>,
    first_removed: usize,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    if facts.range(first_removed..).next().is_none() {
        return Ok(());
    }
    charge(fact_work, facts.len(), pc)?;
    Rc::make_mut(facts).retain(|index, _| *index < first_removed);
    Ok(())
}

fn set_aggregate_shape(
    facts: &mut Rc<BTreeMap<usize, Rc<Vec<FlowValue>>>>,
    slot: usize,
    shape: Option<Rc<Vec<FlowValue>>>,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    if facts.get(&slot).cloned() == shape {
        return Ok(());
    }
    let cost = aggregate_shape_cost(facts)
        .saturating_add(shape.as_ref().map_or(0, |shape| shape.len()))
        .saturating_add(1);
    charge(fact_work, cost, pc)?;
    let facts = Rc::make_mut(facts);
    if let Some(shape) = shape.filter(|shape| {
        (shape.is_empty() || shape.iter().any(|value| !value.is_unknown()))
            && shape.len() <= MAX_TRACKED_AGGREGATE_ITEMS
            && (facts.contains_key(&slot) || facts.len() < MAX_TRACKED_AGGREGATES)
    }) {
        facts.insert(slot, shape);
    } else {
        facts.remove(&slot);
    }
    Ok(())
}

fn truncate_aggregate_shapes(
    facts: &mut Rc<BTreeMap<usize, Rc<Vec<FlowValue>>>>,
    first_removed: usize,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    if facts.range(first_removed..).next().is_none() {
        return Ok(());
    }
    charge(fact_work, aggregate_shape_cost(facts), pc)?;
    Rc::make_mut(facts).retain(|slot, _| *slot < first_removed);
    Ok(())
}

fn set_record_shape(
    facts: &mut Rc<BTreeMap<usize, Rc<BTreeMap<u32, FlowValue>>>>,
    slot: usize,
    shape: Option<Rc<BTreeMap<u32, FlowValue>>>,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    if facts.get(&slot).cloned() == shape {
        return Ok(());
    }
    let cost = record_shape_cost(facts)
        .saturating_add(shape.as_ref().map_or(0, |shape| shape.len()))
        .saturating_add(1);
    charge(fact_work, cost, pc)?;
    let facts = Rc::make_mut(facts);
    if let Some(shape) = shape.filter(|shape| {
        shape.len() <= MAX_TRACKED_AGGREGATE_ITEMS
            && (facts.contains_key(&slot) || facts.len() < MAX_TRACKED_AGGREGATES)
    }) {
        facts.insert(slot, shape);
    } else {
        facts.remove(&slot);
    }
    Ok(())
}

fn truncate_record_shapes(
    facts: &mut Rc<BTreeMap<usize, Rc<BTreeMap<u32, FlowValue>>>>,
    first_removed: usize,
    pc: usize,
    fact_work: &mut usize,
) -> Result<(), String> {
    if facts.range(first_removed..).next().is_none() {
        return Ok(());
    }
    charge(fact_work, record_shape_cost(facts), pc)?;
    Rc::make_mut(facts).retain(|slot, _| *slot < first_removed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_call_contracts_overapproximate_runtime_acceptance() {
        use crate::heap::HeapRef;
        use crate::value::{ArrayValue, RecordValue, RuntimeType as T, ValueFields};
        use std::cell::RefCell;
        use std::collections::HashMap;

        let strings: [Rc<str>; 3] = [
            Rc::from("@struct::demo::A"),
            Rc::from("demo::A::Ready"),
            Rc::from("demo::A"),
        ];
        let values = [
            (Value::Int(1), None),
            (Value::Float(1.0), None),
            (Value::Bool(true), None),
            (Value::Nil, None),
            (Value::Str("text".into()), None),
            (
                Value::StrBuf(HeapRef::try_new(RefCell::new(String::new())).unwrap()),
                None,
            ),
            (
                Value::Record(HeapRef::try_new(RecordValue::new(HashMap::new())).unwrap()),
                None,
            ),
            (
                Value::Record(
                    HeapRef::try_new(RecordValue::nominal(HashMap::new(), "demo::A".into()))
                        .unwrap(),
                ),
                Some(0),
            ),
            (
                Value::Variant(
                    strings[1].clone(),
                    HeapRef::try_new(ValueFields::new(HashMap::new())).unwrap(),
                ),
                Some(1),
            ),
            (Value::TypeTag(strings[2].clone()), Some(2)),
            (Value::Function(1), None),
            (
                Value::Array(
                    HeapRef::try_new(ArrayValue::typed(vec![Value::Int(1)], T::Int).unwrap())
                        .unwrap(),
                ),
                None,
            ),
            (
                Value::Array(
                    HeapRef::try_new(ArrayValue::try_tuple(vec![Value::Bool(true)]).unwrap())
                        .unwrap(),
                ),
                None,
            ),
            (
                Value::Bytes(HeapRef::try_new(RefCell::new(Vec::new())).unwrap()),
                None,
            ),
        ];
        let expected = [
            T::Int,
            T::Float,
            T::Bool,
            T::Nil,
            T::Str,
            T::Record,
            T::Function,
            T::Other,
            T::Nominal("demo::A".into()),
            T::Nominal("demo::B".into()),
            T::Variant("demo::A".into()),
            T::Array(Box::new(T::Int)),
            T::Tuple(vec![T::Bool]),
            T::Nullable(Box::new(T::Int)),
            T::Error("error".into()),
        ];
        for (value, tag) in &values {
            let mut flow = value_flow(value, *tag);
            if matches!(value, Value::Record(_)) {
                flow.known_tag = *tag;
            }
            for expected in &expected {
                let runtime_accepts = expected.accepts(&T::of(value));
                let mut budget = MAX_FACT_WORK;
                let may_match =
                    normal_type_may_match_runtime_type(flow, expected, &strings, 0, &mut budget)
                        .unwrap();
                let definitely_matches = normal_type_definitely_matches_runtime_type(
                    flow,
                    expected,
                    &strings,
                    0,
                    &mut budget,
                )
                .unwrap();
                assert!(
                    !runtime_accepts || may_match,
                    "lost accepted {value:?} at {expected:?}"
                );
                assert!(
                    !definitely_matches || runtime_accepts,
                    "unsound certainty for {value:?} at {expected:?}"
                );
            }
        }
    }

    #[test]
    fn recursive_effect_summaries_invalidate_an_entire_writing_cycle() {
        for writes in [false, true] {
            let mut instructions = vec![
                Instruction::Halt,
                Instruction::Call(3, 0),
                Instruction::Return,
            ];
            if writes {
                instructions.extend([Instruction::Push(Value::Nil), Instruction::StoreGlobal(0)]);
            }
            instructions.extend([Instruction::Call(1, 0), Instruction::Return]);
            let mut program = CompiledProgram::new(instructions);
            for entry in [1, 3] {
                program
                    .callables
                    .insert(entry, CallableSignature::dynamic(0, 0));
            }
            let mut budget = MAX_FACT_WORK;
            let effects = write_free_callables(&program, &mut budget).unwrap();
            assert_eq!(effects.len(), if writes { 0 } else { 2 });
        }
    }

    #[test]
    fn registered_any_results_remain_unknown_even_for_known_arguments() {
        use crate::runtime::{NativeImport, NativeRegistry, NativeType, RegisteredNative};
        let function = Rc::new(RegisteredNative {
            source_name: "host::dynamic".into(),
            identity: "$native::test::host::dynamic".into(),
            version: 1,
            parameters: vec![NativeType::Any],
            result: NativeType::Any,
            mutating_parameters: Vec::new(),
            invoke: None,
        });
        let mut registry = NativeRegistry::default();
        registry
            .names
            .insert(function.source_name.clone(), function.clone());
        registry
            .identities
            .insert(function.identity.clone(), function);
        crate::runtime::with_registry(Rc::new(registry), || {
            let import = NativeImport::named("host::dynamic").unwrap();
            for argument in [FlowValue::integer(1), FlowValue::ERROR] {
                assert_eq!(native_result_flow(&import, argument), FlowValue::UNKNOWN);
            }
        });
    }

    #[test]
    fn record_constructor_key_scan_uses_the_shared_work_budget() {
        let program = CompiledProgram::new(vec![Instruction::MakeRecord(2)]);
        let strings = [Rc::<str>::from("field")];
        let context = VerifyContext {
            program: &program,
            instruction_string_ids: &[],
            strings: &strings,
            callable_results: &BTreeMap::new(),
            write_free_callables: &BTreeSet::new(),
        };
        let state = State {
            height: 4,
            initialized: Rc::new(BTreeSet::new()),
            stack_values: Rc::new(BTreeMap::from([
                (0, FlowValue::string(0)),
                (2, FlowValue::string(0)),
            ])),
            aggregate_shapes: Rc::new(BTreeMap::new()),
            record_shapes: Rc::new(BTreeMap::new()),
            local_values: Rc::new(BTreeMap::new()),
            global_values: Rc::new(BTreeMap::new()),
            capture_values: None,
        };
        let mut budget = 1;

        let error = produced_flow(
            &context,
            &Instruction::MakeRecord(2),
            &state,
            state.height,
            17,
            &mut budget,
        )
        .unwrap_err();

        assert!(error.contains("complexity limit"));
    }

    #[test]
    fn callable_result_and_effect_summaries_share_the_work_budget() {
        let mut program = CompiledProgram::new(vec![
            Instruction::Halt,
            Instruction::Push(Value::Nil),
            Instruction::Return,
        ]);
        program
            .callables
            .insert(1, CallableSignature::dynamic(0, 0));
        let mut budget = 1;
        let error = write_free_callables(&program, &mut budget).unwrap_err();
        assert!(error.contains("complexity limit"));
        let mut budget = 1;
        let error = callable_result_summaries(&program, &[], &[], &BTreeSet::new(), &mut budget)
            .unwrap_err();
        assert!(error.contains("complexity limit"));
    }

    #[test]
    fn call_contract_analysis_charges_parameters_types_and_name_bytes() {
        use crate::value::RuntimeType;
        let expected = RuntimeType::Nullable(Box::new(RuntimeType::Nominal("demo::A".into())));
        let mut budget = 1;
        let error = normal_type_may_match_runtime_type(
            FlowValue::normal(TYPE_RECORD),
            &expected,
            &[],
            0,
            &mut budget,
        )
        .unwrap_err();
        assert!(error.contains("complexity limit"));

        let mut tagged = FlowValue::normal(TYPE_RECORD);
        tagged.known_tag = Some(0);
        let mut budget = 2;
        let error = normal_type_may_match_runtime_type(
            tagged,
            &RuntimeType::Nominal("demo::A".into()),
            &[Rc::from("@struct::demo::A")],
            0,
            &mut budget,
        )
        .unwrap_err();
        assert!(error.contains("complexity limit"));

        let mut types = vec![None; 128];
        types[127] = Some(RuntimeType::Bool);
        let callable = CallableSignature {
            parameters: 128,
            captures: 0,
            parameter_types: Some(types),
            result_type: None,
        };
        let mut budget = 16;
        let error = callable_argument_compatibility(
            &callable,
            std::iter::repeat_n(FlowValue::normal(TYPE_BOOL), 128),
            &[],
            0,
            &mut budget,
        )
        .unwrap_err();
        assert!(error.contains("complexity limit"));
    }

    #[test]
    fn callable_result_flow_charges_nested_result_type_nodes() {
        use crate::value::RuntimeType;

        let result =
            RuntimeType::Nullable(Box::new(RuntimeType::Nullable(Box::new(RuntimeType::Int))));
        let mut program = CompiledProgram::new(vec![
            Instruction::Halt,
            Instruction::Push(Value::Nil),
            Instruction::Return,
        ]);
        program.callables.insert(
            1,
            CallableSignature {
                parameters: 0,
                captures: 0,
                parameter_types: None,
                result_type: Some(result),
            },
        );
        let context = VerifyContext {
            program: &program,
            instruction_string_ids: &[],
            strings: &[],
            callable_results: &BTreeMap::new(),
            write_free_callables: &BTreeSet::new(),
        };
        let mut budget = 1;
        let error =
            callable_result_flow(&context, 1, std::iter::empty(), 0, &mut budget).unwrap_err();

        assert!(error.contains("complexity limit"));
    }

    #[test]
    fn instruction_string_interning_charges_the_bytes_it_hashes() {
        let program = CompiledProgram::new(vec![Instruction::Push(Value::Str(Rc::from(
            "a long bytecode string constant",
        )))]);
        let mut budget = 8;

        let error = intern_instruction_strings(&program, &mut budget).unwrap_err();

        assert!(error.contains("complexity limit"));
    }

    #[test]
    fn nominal_type_test_charges_tag_comparison_bytes() {
        let strings = [Rc::<str>::from("demo::AQuiteLongVariant")];
        let mut value = FlowValue::normal(TYPE_VARIANT);
        value.known_tag = Some(0);
        let mut budget = 1;

        let error = type_test_flow(
            &crate::instruction::ValueType::Enum(Rc::from("demo")),
            value,
            &strings,
            11,
            &mut budget,
        )
        .unwrap_err();

        assert!(error.contains("complexity limit"));
    }

    #[test]
    fn analysis_rejects_excess_work_with_a_diagnostic() {
        let mut instructions = Vec::new();
        for slot in 0..16 {
            instructions.extend([Instruction::Push(Value::Nil), Instruction::Store(slot)]);
        }
        instructions.push(Instruction::Halt);
        let program = CompiledProgram::new(instructions);
        let context = VerifyContext {
            program: &program,
            instruction_string_ids: &[],
            strings: &[],
            callable_results: &BTreeMap::new(),
            write_free_callables: &BTreeSet::new(),
        };
        let mut closure_outputs = BTreeMap::new();
        let mut fact_work = 32;
        let mut visits = 100;
        let mut flow = FlowAnalysis {
            capture_values: None,
            closure_outputs: &mut closure_outputs,
            fact_work: &mut fact_work,
            visits: &mut visits,
        };
        let error = region(&context, 0, program.instructions.len(), 0, 0, &mut flow).unwrap_err();
        assert!(error.contains("complexity limit"));
        let mut closure_outputs = BTreeMap::new();
        let mut fact_work = 1000;
        let mut visits = 4;
        let mut flow = FlowAnalysis {
            capture_values: None,
            closure_outputs: &mut closure_outputs,
            fact_work: &mut fact_work,
            visits: &mut visits,
        };
        let error = region(&context, 0, program.instructions.len(), 0, 0, &mut flow).unwrap_err();
        assert!(error.contains("complexity limit"));
    }

    #[test]
    fn full_region_scans_use_the_visit_budget() {
        let program = CompiledProgram::new(
            std::iter::repeat_with(|| Instruction::Halt)
                .take(10)
                .collect(),
        );
        let context = VerifyContext {
            program: &program,
            instruction_string_ids: &[],
            strings: &[],
            callable_results: &BTreeMap::new(),
            write_free_callables: &BTreeSet::new(),
        };
        let mut closure_outputs = BTreeMap::new();
        let mut fact_work = MAX_FACT_WORK;
        // Before charging the full scans, node visits alone fit in this
        // budget. The repeated linear passes must now reject predictably.
        let mut visits = 12;
        let mut flow = FlowAnalysis {
            capture_values: None,
            closure_outputs: &mut closure_outputs,
            fact_work: &mut fact_work,
            visits: &mut visits,
        };

        let error = region(&context, 0, 10, 0, 0, &mut flow).unwrap_err();

        assert!(error.contains("complexity limit"));
    }
}
