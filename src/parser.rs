extern crate time;

use nom::{digit, anychar, IResult, Err};
use std::str::{self, FromStr};
use std::collections::{HashMap, HashSet};
use std::collections::hash_map::Entry;
use ops::{Interner, Field, Constraint, register, Program, make_scan, make_anti_scan,
          make_intermediate_insert, make_intermediate_scan, make_filter, make_function,
          make_multi_function, Block};
use std::io::prelude::*;
use std::fs::File;

struct FunctionInfo {
    is_multi: bool,
    params: Vec<String>,
    outputs: Vec<String>,
}

enum ParamType {
    Param(usize),
    Output(usize),
    Invalid,
}

impl FunctionInfo {
    pub fn new(raw_params:Vec<&str>) -> FunctionInfo {
        let params = raw_params.iter().map(|s| s.to_string()).collect();
        FunctionInfo { is_multi:false, params, outputs: vec![] }
    }

    pub fn multi(raw_params:Vec<&str>, raw_outputs:Vec<&str>) -> FunctionInfo {
        let params = raw_params.iter().map(|s| s.to_string()).collect();
        let outputs = raw_outputs.iter().map(|s| s.to_string()).collect();
        FunctionInfo { is_multi:true, params, outputs }
    }

    pub fn get_index(&self, param:&str) -> ParamType {
        if let Some(v) = self.params.iter().enumerate().find(|&(_, t)| t == param) {
            ParamType::Param(v.0)
        } else if let Some(v) = self.outputs.iter().enumerate().find(|&(_, t)| t == param) {
            ParamType::Output(v.0)
        } else {
            ParamType::Invalid
        }
    }
}

lazy_static! {
    static ref FUNCTION_INFO: HashMap<String, FunctionInfo> = {
        let mut m = HashMap::new();
        let mut info = HashMap::new();
        info.insert("degrees".to_string(), 0);
        m.insert("math/sin".to_string(), FunctionInfo::new(vec!["degrees"]));
        m.insert("math/cos".to_string(), FunctionInfo::new(vec!["degrees"]));
        m.insert("string/split".to_string(), FunctionInfo::multi(vec!["text", "by"], vec!["token", "index"]));
        m
    };
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    Bind,
    Commit,
}

#[derive(Debug, Clone)]
pub enum Node<'a> {
    Pipe,
    Integer(i32),
    Float(f32),
    RawString(&'a str),
    EmbeddedString(Option<String>, Vec<Node<'a>>),
    ExprSet(Vec<Node<'a>>),
    NoneValue,
    Tag(&'a str),
    Variable(&'a str),
    GeneratedVariable(String),
    Attribute(&'a str),
    AttributeEquality(&'a str, Box<Node<'a>>),
    AttributeInequality {attribute:&'a str, right:Box<Node<'a>>, op:&'a str},
    AttributeAccess(Vec<&'a str>),
    MutatingAttributeAccess(Vec<&'a str>),
    Inequality {left:Box<Node<'a>>, right:Box<Node<'a>>, op:&'a str},
    Equality {left:Box<Node<'a>>, right:Box<Node<'a>>},
    Infix {result:Option<String>, left:Box<Node<'a>>, right:Box<Node<'a>>, op:&'a str},
    Record(Option<String>, Vec<Node<'a>>),
    RecordSet(Vec<Node<'a>>),
    RecordFunction { op:&'a str, params:Vec<Node<'a>>, outputs:Vec<Node<'a>> },
    OutputRecord(Option<String>, Vec<Node<'a>>, OutputType),
    RecordUpdate {record:Box<Node<'a>>, value:Box<Node<'a>>, op:&'a str, output_type:OutputType},
    Not(usize, Vec<Node<'a>>),
    IfBranch { sub_block_id: usize, exclusive:bool, result:Box<Node<'a>>, body:Vec<Node<'a>> },
    If { sub_block_id:usize, exclusive:bool, outputs:Option<Vec<Node<'a>>>, branches:Vec<Node<'a>> },
    Search(Vec<Node<'a>>),
    Bind(Vec<Node<'a>>),
    Commit(Vec<Node<'a>>),
    Project(Vec<Node<'a>>),
    Watch(&'a str, Vec<Node<'a>>),
    Block{search:Box<Option<Node<'a>>>, update:Box<Node<'a>>},
    Doc { file:String, blocks:Vec<Node<'a>> }
}

#[derive(Debug, Clone)]
pub enum SubBlock {
    Not(Compilation),
    IfBranch(Compilation, Vec<Field>),
    If(Compilation, Vec<Field>, bool),
}

impl SubBlock {
    pub fn get_mut_compilation(&mut self) -> &mut Compilation {
        match self {
            &mut SubBlock::Not(ref mut comp) => comp,
            &mut SubBlock::IfBranch(ref mut comp, ..) => comp,
            &mut SubBlock::If(ref mut comp, ..) => comp,
        }
    }
}

impl<'a> Node<'a> {

    pub fn unify(&self, comp:&mut Compilation) {
        {
            let ref mut values:HashMap<Field, Field> = comp.var_values;
            let ref mut unified_registers:HashMap<Field, Field> = comp.unified_registers;
            let mut provided = HashSet::new();
            for v in comp.vars.values() {
                let field = Field::Register(*v);
                values.insert(field, field);
                unified_registers.insert(field, field);
                if comp.provided_registers.contains(&field) {
                    provided.insert(field);
                }
            }
            let mut changed = true;
            // go in rounds and try to unify everything
            while changed {
                changed = false;
                for &(l, r) in comp.equalities.iter() {
                    match (l, r) {
                        (Field::Register(l_reg), Field::Register(r_reg)) => {
                            if l_reg < r_reg {
                                unified_registers.insert(r, l.clone());
                            } else if r_reg < l_reg {
                                unified_registers.insert(l, r.clone());
                            }
                        }
                        _ => {}
                    }

                    let left_value:Field = if let Field::Register(_) = l { values.entry(l).or_insert(l).clone() } else { l };
                    let right_value:Field = if let Field::Register(_) = r { values.entry(r).or_insert(r).clone() } else { r };
                    match (left_value, right_value) {
                        (Field::Register(l_reg), Field::Register(r_reg)) => {
                            if l_reg < r_reg {
                                values.insert(r, left_value.clone());
                                unified_registers.insert(r, left_value.clone());
                                if provided.contains(&left_value) {
                                    provided.insert(r);
                                }
                                changed = true;
                            } else if r_reg < l_reg {
                                values.insert(l, right_value.clone());
                                unified_registers.insert(l, right_value.clone());
                                if provided.contains(&right_value) {
                                    provided.insert(l);
                                }
                                changed = true;
                            }
                        },
                        (Field::Register(_), other) => {
                            values.insert(l, other.clone());
                            provided.insert(l);
                            changed = true;
                        },
                        (other, Field::Register(_)) => {
                            values.insert(r, other.clone());
                            provided.insert(r);
                            changed = true;
                        },
                        (a, b) => { if a != b { panic!("Invalid equality {:?} != {:?}", a, b); } },
                    }
                }
            }
            comp.provided_registers = provided;
            comp.required_fields = comp.required_fields.iter().map(|v| unified_registers.get(v).unwrap().clone()).collect();
            println!("REQUIRED: {:?}", comp.required_fields);
        }


        for sub_block in comp.sub_blocks.iter_mut() {
            match sub_block {
                &mut SubBlock::Not(ref mut sub_comp) |
                &mut SubBlock::IfBranch(ref mut sub_comp, ..) |
                &mut SubBlock::If(ref mut sub_comp, ..) => {
                    println!("   VARS {:?}", comp.vars);
                    println!("   SUB_VARS {:?}", sub_comp.vars);
                    // transfer values
                    for (k, v) in comp.vars.iter() {
                        match sub_comp.vars.entry(k.to_string()) {
                            Entry::Occupied(o) => {
                                let reg = o.get();
                                sub_comp.equalities.push((Field::Register(*v), Field::Register(*reg)));
                                println!("SETTING EQUAL: {:?}", (Field::Register(*v), Field::Register(*reg)));
                            }
                            Entry::Vacant(o) => {
                                o.insert(*v);
                            }
                        }
                    }
                    sub_comp.var_values = comp.var_values.clone();
                    self.unify(sub_comp);
                }
            }
        }
    }

    pub fn gather_equalities(&mut self, interner:&mut Interner, cur_block:&mut Compilation) -> Option<Field> {
        match self {
            &mut Node::Pipe => { None },
            &mut Node::Tag(_) => { None },
            &mut Node::Integer(v) => { Some(interner.number(v as f32)) }
            &mut Node::Float(v) => { Some(interner.number(v)) },
            &mut Node::RawString(v) => { Some(interner.string(v)) },
            &mut Node::Variable(v) => { Some(cur_block.get_register(v)) },
            &mut Node::GeneratedVariable(ref v) => { Some(cur_block.get_register(v)) },
            &mut Node::NoneValue => { None },
            &mut Node::Attribute(a) => { Some(cur_block.get_register(a)) },
            &mut Node::AttributeInequality {ref mut right, ..} => { right.gather_equalities(interner, cur_block) },
            &mut Node::AttributeEquality(_, ref mut v) => { v.gather_equalities(interner, cur_block) },
            &mut Node::Inequality {ref mut left, ref mut right, ..} => {
                left.gather_equalities(interner, cur_block);
                right.gather_equalities(interner, cur_block);
                None
            },
            &mut Node::EmbeddedString(ref mut var, ref mut vs) => {
                for v in vs {
                    v.gather_equalities(interner, cur_block);
                }
                let var_name = format!("__eve_concat{}", cur_block.id);
                cur_block.id += 1;
                let reg = cur_block.get_register(&var_name);
                *var = Some(var_name);
                Some(reg)

            },
            &mut Node::Equality {ref mut left, ref mut right} => {
                let l = left.gather_equalities(interner, cur_block).unwrap();
                let r = right.gather_equalities(interner, cur_block).unwrap();
                cur_block.equalities.push((l,r));
                if cur_block.is_child {
                    if let Field::Register(_) = l { cur_block.required_fields.push(l); }
                    if let Field::Register(_) = r { cur_block.required_fields.push(r); }
                }
                None
            },
            &mut Node::ExprSet(ref mut items) => {
                for expr in items {
                    expr.gather_equalities(interner, cur_block);
                }
                None
            },
            &mut Node::Infix {ref mut result, ref mut left, ref mut right, ..} => {
                left.gather_equalities(interner, cur_block);
                right.gather_equalities(interner, cur_block);
                let result_name = format!("__eve_infix{}", cur_block.id);
                cur_block.id += 1;
                let reg = cur_block.get_register(&result_name);
                *result = Some(result_name);
                Some(reg)
            },
            &mut Node::RecordFunction {ref mut params, ref mut outputs, ..} => {
                for param in params.iter_mut() {
                    param.gather_equalities(interner, cur_block);
                }
                if outputs.len() == 0 {
                    let result_name = format!("__eve_infix{}", cur_block.id);
                    outputs.push(Node::GeneratedVariable(result_name));
                    cur_block.id += 1;
                }
                let outs:Vec<Option<Field>> = outputs.iter_mut().map(|output| output.gather_equalities(interner, cur_block)).collect();
                *outs.get(0).unwrap()
            },
            &mut Node::RecordSet(ref mut records) => {
                for record in records {
                    record.gather_equalities(interner, cur_block);
                }
                None
            },
            &mut Node::Record(ref mut var, ref mut attrs) => {
                for attr in attrs {
                    attr.gather_equalities(interner, cur_block);
                }
                let var_name = format!("__eve_record{}", cur_block.id);
                cur_block.id += 1;
                let reg = cur_block.get_register(&var_name);
                *var = Some(var_name);
                Some(reg)
            },
            &mut Node::OutputRecord(ref mut var, ref mut attrs, ..) => {
                for attr in attrs {
                    attr.gather_equalities(interner, cur_block);
                }
                let var_name = format!("__eve_output_record{}", cur_block.id);
                cur_block.id += 1;
                let reg = cur_block.get_register(&var_name);
                *var = Some(var_name);
                Some(reg)
            },
            &mut Node::AttributeAccess(ref items) => {
                let mut final_var = "attr_access".to_string();
                for item in items {
                    final_var.push_str("|");
                    final_var.push_str(item);
                }
                let reg = cur_block.get_register(&final_var);
                Some(reg)
            },
            &mut Node::MutatingAttributeAccess(_) => {
                None
            },
            &mut Node::RecordUpdate {ref mut record, ref op, ref mut value, ..} => {
                let left = record.gather_equalities(interner, cur_block);
                let right = value.gather_equalities(interner, cur_block);
                if op == &"<-" {
                    cur_block.provide(right.unwrap());
                    cur_block.equalities.push((left.unwrap(), right.unwrap()));
                }
                None
            },
            &mut Node::Not(ref mut sub_id, ref mut items) => {
                let mut sub_block = Compilation::new_child(cur_block);
                sub_block.id = cur_block.id + 10000;
                for item in items {
                    item.gather_equalities(interner, &mut sub_block);
                };
                *sub_id = cur_block.sub_blocks.len();
                cur_block.sub_blocks.push(SubBlock::Not(sub_block));
                None
            },
            &mut Node::IfBranch {ref mut sub_block_id, ref mut body, ref mut result, ..} => {
                let mut sub_block = Compilation::new_child(cur_block);
                for item in body {
                    item.gather_equalities(interner, &mut sub_block);
                };
                result.gather_equalities(interner, &mut sub_block);
                *sub_block_id = cur_block.sub_blocks.len();
                cur_block.sub_blocks.push(SubBlock::IfBranch(sub_block, vec![]));
                None
            },
            &mut Node::If {ref mut sub_block_id, ref mut branches, ref mut outputs, exclusive, ..} => {
                let mut sub_block = Compilation::new_child(cur_block);
                if let &mut Some(ref mut outs) = outputs {
                    for out in outs {
                        out.gather_equalities(interner, cur_block);
                    };
                }
                for branch in branches {
                    branch.gather_equalities(interner, &mut sub_block);
                };
                *sub_block_id = cur_block.sub_blocks.len();
                cur_block.sub_blocks.push(SubBlock::If(sub_block, vec![], exclusive));
                None
            },
            &mut Node::Search(ref mut statements) => {
                for s in statements {
                    s.gather_equalities(interner, cur_block);
                };
                None
            },
            &mut Node::Bind(ref mut statements) => {
                for s in statements {
                    s.gather_equalities(interner, cur_block);
                };
                None
            },
            &mut Node::Commit(ref mut statements) => {
                for s in statements {
                    s.gather_equalities(interner, cur_block);
                };
                None
            },
            &mut Node::Project(ref mut values) => {
                for v in values {
                    v.gather_equalities(interner, cur_block);
                };
                None
            },
            &mut Node::Watch(_, ref mut values) => {
                for v in values {
                    v.gather_equalities(interner, cur_block);
                };
                None
            },
            &mut Node::Block{ref mut search, ref mut update} => {
                if let Some(ref mut s) = **search {
                    s.gather_equalities(interner, cur_block);
                };
                update.gather_equalities(interner, cur_block);
                None
            },
            _ => panic!("Trying to gather equalities on {:?}", self)
        }
    }

    pub fn compile(&self, interner:&mut Interner, cur_block: &mut Compilation) -> Option<Field> {
        match self {
            &Node::Integer(v) => { Some(interner.number(v as f32)) }
            &Node::Float(v) => { Some(interner.number(v)) },
            &Node::RawString(v) => { Some(interner.string(v)) },
            &Node::Variable(v) => { Some(cur_block.get_unified_register(v)) },
            &Node::GeneratedVariable(ref v) => { Some(cur_block.get_unified_register(v)) },
            // &Node::AttributeEquality(a, ref v) => { v.compile(interner, comp, cur_block) },
            &Node::Equality {ref left, ref right} => {
                left.compile(interner, cur_block);
                right.compile(interner, cur_block);
                None
            },
            &Node::AttributeAccess(ref items) => {
                let mut final_var = "attr_access".to_string();
                let mut parent = cur_block.get_unified_register(items[0]);
                for item in items[1..].iter() {
                    final_var.push_str("|");
                    final_var.push_str(item);
                    let next = cur_block.get_unified_register(&final_var.to_string());
                    cur_block.constraints.push(make_scan(parent, interner.string(item), next));
                    parent = next;
                }
                Some(parent)
            },
            &Node::MutatingAttributeAccess(ref items) => {
                let mut final_var = "attr_access".to_string();
                let mut parent = cur_block.get_unified_register(items[0]);
                if items.len() > 2 {
                    for item in items[1..items.len()-2].iter() {
                        final_var.push_str("|");
                        final_var.push_str(item);
                        let next = cur_block.get_unified_register(&final_var.to_string());
                        cur_block.constraints.push(make_scan(parent, interner.string(item), next));
                        parent = next;
                    }
                }
                Some(parent)
            },
            &Node::Inequality {ref left, ref right, ref op} => {
                let left_value = left.compile(interner, cur_block);
                let right_value = right.compile(interner, cur_block);
                match (left_value, right_value) {
                    (Some(l), Some(r)) => {
                        cur_block.constraints.push(make_filter(op, l, r));
                    },
                    _ => panic!("inequality without both a left and right: {:?} {} {:?}", left, op, right)
                }
                right_value
            },
            &Node::EmbeddedString(ref var, ref vs) => {
                let resolved = vs.iter().map(|v| v.compile(interner, cur_block).unwrap()).collect();
                if let &Some(ref name) = var {
                    let mut out_reg = cur_block.get_register(name);
                    let out_value = cur_block.get_value(name);
                    if let Field::Register(_) = out_value {
                        out_reg = out_value;
                    } else {
                        cur_block.constraints.push(make_filter("=", out_reg, out_value));
                    }
                    cur_block.constraints.push(make_function("concat", resolved, out_reg));
                    Some(out_reg)
                } else {
                    panic!("Embedded string without a result assigned {:?}", self);
                }

            },
            &Node::Infix { ref op, ref result, ref left, ref right } => {
                let left_value = left.compile(interner, cur_block).unwrap();
                let right_value = right.compile(interner, cur_block).unwrap();
                if let &Some(ref name) = result {
                    let mut out_reg = cur_block.get_register(name);
                    let out_value = cur_block.get_value(name);
                    if let Field::Register(_) = out_value {
                        out_reg = out_value;
                    } else {
                        cur_block.constraints.push(make_filter("=", out_reg, out_value));
                    }
                    cur_block.constraints.push(make_function(op, vec![left_value, right_value], out_reg));
                    Some(out_reg)
                } else {
                    panic!("Infix without a result assigned {:?}", self);
                }
            },
            &Node::RecordFunction { ref op, ref params, ref outputs} => {
                let info = FUNCTION_INFO.get(*op).unwrap();
                let mut cur_outputs = vec![Field::Value(0); info.outputs.len()];
                let mut cur_params = vec![Field::Value(0); info.params.len()];
                for param in params {
                    let (a, v) = match param {
                        &Node::Attribute(a) => {
                            (a, cur_block.get_value(a))
                        }
                        &Node::AttributeEquality(a, ref v) => {
                            (a, v.compile(interner, cur_block).unwrap())
                        }
                        _ => { panic!("invalid function param: {:?}", param) }
                    };
                    match info.get_index(a) {
                        ParamType::Param(ix) => { cur_params[ix] = v; }
                        ParamType::Output(ix) => { cur_outputs[ix] = v; }
                        ParamType::Invalid => { panic!("Invalid parameter for function: {:?} - {:?}", op, a) }
                    }
                }
                let compiled_outputs:Vec<Option<Field>> = outputs.iter().map(|output| output.compile(interner, cur_block)).collect();
                for (out_ix, mut attr_output) in cur_outputs.iter_mut().enumerate() {
                    let maybe_output = compiled_outputs.get(out_ix).map(|x| x.unwrap());
                    match (&attr_output, maybe_output) {
                        (&&mut Field::Value(0), Some(Field::Register(_))) => {
                            *attr_output = maybe_output.unwrap();
                        },
                        (&&mut Field::Value(0), Some(Field::Value(_))) => {
                            let result_name = format!("__eve_record_function_output{}", cur_block.id);
                            let out_reg = cur_block.get_register(&result_name);
                            cur_block.id += 1;
                            cur_block.constraints.push(make_filter("=", out_reg, maybe_output.unwrap()));
                            *attr_output = out_reg;
                        },
                        (&&mut Field::Value(_), Some(Field::Register(_))) => {
                            cur_block.constraints.push(make_filter("=", *attr_output, maybe_output.unwrap()));
                            *attr_output = maybe_output.unwrap();
                        },
                        (&&mut Field::Register(_), Some(Field::Value(_))) |
                        (&&mut Field::Register(_), Some(Field::Register(_))) => {
                            cur_block.constraints.push(make_filter("=", *attr_output, maybe_output.unwrap()));
                        },
                        (&&mut Field::Value(x), None) => {
                            let result_name = format!("__eve_record_function_output{}", cur_block.id);
                            let out_reg = cur_block.get_register(&result_name);
                            cur_block.id += 1;
                            if x > 0 {
                                cur_block.constraints.push(make_filter("=", *attr_output, out_reg));
                            }
                            *attr_output = out_reg;
                        },
                        (&&mut Field::Value(x), Some(Field::Value(z))) => {
                            if x != z { panic!("Invalid constant equality in record function: {:?} != {:?}", x, z) }
                            let result_name = format!("__eve_record_function_output{}", cur_block.id);
                            let out_reg = cur_block.get_register(&result_name);
                            cur_block.id += 1;
                            if x > 0 {
                                cur_block.constraints.push(make_filter("=", *attr_output, out_reg));
                            }
                            *attr_output = out_reg;
                        },
                        _ => { }
                    }
                }
                let final_result = Some(cur_outputs[0].clone());
                if info.is_multi {
                    cur_block.constraints.push(make_multi_function(op, cur_params, cur_outputs));
                } else {
                    cur_block.constraints.push(make_function(op, cur_params, cur_outputs[0]));
                }
                final_result
            },
            &Node::Record(ref var, ref attrs) => {
                let reg = if let &Some(ref name) = var {
                    cur_block.get_unified_register(name)
                } else {
                    panic!("Record missing a var {:?}", var)
                };
                for attr in attrs {
                    let (a, v) = match attr {
                        &Node::Tag(t) => { (interner.string("tag"), interner.string(t)) },
                        &Node::Attribute(a) => { (interner.string(a), cur_block.get_unified_register(a)) },
                        &Node::AttributeEquality(a, ref v) => {
                            let result_a = interner.string(a);
                            let result = match **v {
                                Node::RecordSet(ref records) => {
                                    for record in records[1..].iter() {
                                        let cur_v = record.compile(interner, cur_block).unwrap();
                                        cur_block.constraints.push(make_scan(reg, result_a, cur_v));
                                    }
                                    records[0].compile(interner, cur_block).unwrap()
                                },
                                Node::ExprSet(ref items) => {
                                    for value in items[1..].iter() {
                                        let cur_v = value.compile(interner, cur_block).unwrap();
                                        cur_block.constraints.push(make_scan(reg, result_a, cur_v));
                                    }
                                    items[0].compile(interner, cur_block).unwrap()
                                },
                                _ => v.compile(interner, cur_block).unwrap()
                            };
                            (result_a, result)
                        },
                        &Node::AttributeInequality {ref attribute, ref op, ref right } => {
                            let reg = cur_block.get_unified_register(attribute);
                            let right_value = right.compile(interner, cur_block);
                            match right_value {
                                Some(r) => {
                                    cur_block.constraints.push(make_filter(op, reg, r));
                                },
                                _ => panic!("inequality without both a left and right: {} {} {:?}", attribute, op, right)
                            }
                            (interner.string(attribute), reg)
                        },
                        _ => { panic!("TODO") }
                    };
                    cur_block.constraints.push(make_scan(reg, a, v));
                };
                Some(reg)
            },
            &Node::OutputRecord(ref var, ref attrs, ref output_type) => {
                let (reg, needs_id) = if let &Some(ref name) = var {
                    (cur_block.get_unified_register(name), !cur_block.is_provided(name))
                } else {
                    panic!("Record missing a var {:?}", var)
                };
                let commit = *output_type == OutputType::Commit;
                let mut identity_contributing = true;
                let mut identity_attrs = vec![];
                for attr in attrs {
                    if let &Node::Pipe = attr {
                        identity_contributing = false;
                        continue;
                    }
                    let (a, v) = match attr {
                        &Node::Tag(t) => { (interner.string("tag"), interner.string(t)) },
                        &Node::Attribute(a) => { (interner.string(a), cur_block.get_unified_register(a)) },
                        &Node::AttributeEquality(a, ref v) => {
                            let result_a = interner.string(a);
                            let result = match **v {
                                Node::RecordSet(ref records) => {
                                    for record in records[1..].iter() {
                                        let cur_v = record.compile(interner, cur_block).unwrap();
                                        cur_block.constraints.push(Constraint::Insert{e:reg, a:result_a, v:cur_v, commit});
                                    }
                                    records[0].compile(interner, cur_block).unwrap()
                                },
                                Node::ExprSet(ref items) => {
                                    for value in items[1..].iter() {
                                        let cur_v = value.compile(interner, cur_block).unwrap();
                                        cur_block.constraints.push(Constraint::Insert{e:reg, a:result_a, v:cur_v, commit});
                                    }
                                    items[0].compile(interner, cur_block).unwrap()
                                },
                                _ => v.compile(interner, cur_block).unwrap()
                            };

                            (result_a, result)
                        },
                        _ => { panic!("TODO") }
                    };
                    if identity_contributing {
                        identity_attrs.push(v);
                    }
                    cur_block.constraints.push(Constraint::Insert{e:reg, a, v, commit});
                };
                if needs_id {
                    cur_block.constraints.push(make_function("gen_id", identity_attrs, reg));
                }
                Some(reg)
            },
            &Node::RecordUpdate {ref record, ref op, ref value, ref output_type} => {
                // @TODO: compile attribute access correctly
                let (reg, attr) = match **record {
                    Node::MutatingAttributeAccess(ref items) => {
                        let parent = record.compile(interner, cur_block);
                        (parent.unwrap(), Some(items[items.len() - 1]))
                    },
                    Node::Variable(v) => {
                        (cur_block.get_unified_register(v), None)
                    },
                    _ => panic!("Invalid record on {:?}", self)
                };
                let commit = *output_type == OutputType::Commit;
                let ref val = **value;
                let (a, v) = match (attr, val) {
                    (None, &Node::Tag(t)) => { (interner.string("tag"), interner.string(t)) },
                    (None, &Node::NoneValue) => { (Field::Value(0), Field::Value(0)) }
                    (Some(attr), &Node::NoneValue) => { (interner.string(attr), Field::Value(0)) }
                    (Some(attr), v) => {
                        (interner.string(attr), v.compile(interner, cur_block).unwrap())
                    },
                    // @TODO: this doesn't handle the case where you do
                    // foo.bar <- [#zomg a]
                    (None, &Node::OutputRecord(..)) => {
                        match op {
                            &"<-" => {
                                val.compile(interner, cur_block);
                                (Field::Value(0), Field::Value(0))
                            }
                            _ => panic!("Invalid {:?}", self)
                        }
                    }
                    _ => { panic!("Invalid {:?}", self) }
                };
                match (*op, a, v) {
                    (":=", Field::Value(0), Field::Value(0)) => {
                        cur_block.constraints.push(Constraint::RemoveEntity {e:reg });
                    },
                    (":=", _, Field::Value(0)) => {
                        cur_block.constraints.push(Constraint::RemoveAttribute {e:reg, a });
                    },
                    (":=", _, _) => {
                        cur_block.constraints.push(Constraint::RemoveAttribute {e:reg, a });
                        cur_block.constraints.push(Constraint::Insert {e:reg, a, v, commit});
                    },
                    (_, Field::Value(0), Field::Value(0)) => {  }
                    ("+=", _, _) => { cur_block.constraints.push(Constraint::Insert {e:reg, a, v, commit}); }
                    ("-=", _, _) => { cur_block.constraints.push(Constraint::Remove {e:reg, a, v }); }
                    _ => { panic!("Invalid record update {:?} {:?} {:?}", op, a, v) }
                }
                Some(reg)
            },
            &Node::Not(sub_block_id, ref items) => {
                let sub_block = if let SubBlock::Not(ref mut sub) = cur_block.sub_blocks[sub_block_id] {
                    sub
                } else {
                    panic!("Wrong SubBlock type for Not");
                };
                for item in items {
                    item.compile(interner, sub_block);
                };
                None
            },
            &Node::IfBranch { sub_block_id, ref body, ref result, ..} => {
                if let SubBlock::IfBranch(ref mut sub_block, ref mut result_fields) = cur_block.sub_blocks[sub_block_id] {
                    for item in body {
                        item.compile(interner, sub_block);
                    };
                    if let Node::ExprSet(ref nodes) = **result {
                        for node in nodes {
                            result_fields.push(node.compile(interner, sub_block).unwrap());
                        }
                    } else {
                        result_fields.push(result.compile(interner, sub_block).unwrap());
                    }
                } else {
                    panic!("Wrong SubBlock type for Not");
                };
                None
            },
            &Node::If { sub_block_id, ref branches, ref outputs, ..} => {
                let compiled_outputs = if let &Some(ref outs) = outputs {
                    outs.iter().map(|cur| {
                        match cur.compile(interner, cur_block) {
                            Some(val @ Field::Value(_)) => {
                                let result_name = format!("__eve_if_output{}", cur_block.id);
                                let out_reg = cur_block.get_register(&result_name);
                                cur_block.id += 1;
                                cur_block.constraints.push(make_filter("=", out_reg, val));
                                out_reg
                            },
                            Some(reg @ Field::Register(_)) => {
                                let cur_value = if let Some(val @ &Field::Value(_)) = cur_block.var_values.get(&reg) {
                                    *val
                                } else {
                                    reg
                                };
                                if let Field::Value(_) = cur_value {
                                    let result_name = format!("__eve_if_output{}", cur_block.id);
                                    let out_reg = cur_block.get_register(&result_name);
                                    cur_block.id += 1;
                                    cur_block.constraints.push(make_filter("=", out_reg, cur_value));
                                    out_reg
                                } else {
                                    reg
                                }
                            },
                            _ => { panic!("Non-value, non-register if output") }
                        }
                    }).collect()
                } else {
                    vec![]
                };
                if let SubBlock::If(ref mut sub_block, ref mut out_registers, ..) = cur_block.sub_blocks[sub_block_id] {
                    out_registers.extend(compiled_outputs);
                    for branch in branches {
                        branch.compile(interner, sub_block);
                    }
                }
                None
            },
            &Node::Search(ref statements) => {
                for s in statements {
                    s.compile(interner, cur_block);
                };
                None
            },
            &Node::Bind(ref statements) => {
                for s in statements {
                    s.compile(interner, cur_block);
                };
                None
            },
            &Node::Commit(ref statements) => {
                for s in statements {
                    s.compile(interner, cur_block);
                };
                None
            },
            &Node::Project(ref values) => {
                let registers = values.iter()
                                      .map(|v| v.compile(interner, cur_block))
                                      .filter(|v| if let &Some(Field::Register(_)) = v { true } else { false })
                                      .map(|v| if let Some(Field::Register(reg)) = v { reg } else { panic!() })
                                      .collect();
                cur_block.constraints.push(Constraint::Project {registers});
                None
            },
            &Node::Watch(ref name, ref values) => {
                let registers = values.iter()
                                      .map(|v| v.compile(interner, cur_block))
                                      .filter(|v| if let &Some(Field::Register(_)) = v { true } else { false })
                                      .map(|v| if let Some(Field::Register(reg)) = v { reg } else { panic!() })
                                      .collect();
                cur_block.constraints.push(Constraint::Watch {name:name.to_string(), registers});
                None
            },
            &Node::Block{ref search, ref update} => {
                if let Some(ref s) = **search {
                    s.compile(interner, cur_block);
                };
                update.compile(interner, cur_block);
                for (ix, mut sub) in cur_block.sub_blocks.iter_mut().enumerate() {
                    let ancestors = cur_block.constraints.clone();
                    self.sub_block(interner, sub, ix, &mut cur_block.constraints, &ancestors);
                }
                None
            },
            _ => panic!("Trying to compile something we don't know how to compile {:?}", self)
        }
    }

    pub fn sub_block(&self, interner:&mut Interner, block:&mut SubBlock, ix:usize, parent_constraints: &mut Vec<Constraint>, ancestor_constraints: &Vec<Constraint>) -> HashSet<Field> {
        match block {
            &mut SubBlock::Not(ref mut cur_block) => {
                let mut inputs = cur_block.get_inputs(parent_constraints);
                if cur_block.sub_blocks.len() > 0 {
                    let mut next_ancestors = ancestor_constraints.clone();
                    next_ancestors.extend(cur_block.constraints.iter().cloned());
                    for sub in cur_block.sub_blocks.iter_mut() {
                        let sub_inputs = self.sub_block(interner, sub, ix, &mut cur_block.constraints, &mut next_ancestors);
                        inputs.extend(sub_inputs);
                    }
                }
                let mut related = get_input_constraints(&inputs, ancestor_constraints);
                related.extend(cur_block.constraints.iter().cloned());
                let block_name = cur_block.block_name.to_string();
                let tag_value = interner.string(&format!("{}|sub_block|not|{}", block_name, ix));
                let mut key_attrs = vec![tag_value];
                key_attrs.extend(inputs.iter());
                parent_constraints.push(make_anti_scan(key_attrs.clone()));
                related.push(make_intermediate_insert(key_attrs, vec![], true));
                cur_block.constraints = related;
                inputs
            }
            &mut SubBlock::IfBranch(..) => { panic!("Tried directly compiling an if branch") }
            &mut SubBlock::If(ref mut cur_block, ref output_registers, exclusive) => {
                // find the inputs for all of the branches
                let mut all_inputs = HashSet::new();
                for sub in cur_block.sub_blocks.iter_mut() {
                    let branch_block = sub.get_mut_compilation();
                    let mut inputs = branch_block.get_inputs(parent_constraints);
                    if branch_block.sub_blocks.len() > 0 {
                        let mut next_ancestors = ancestor_constraints.clone();
                        next_ancestors.extend(branch_block.constraints.iter().cloned());
                        for sub in branch_block.sub_blocks.iter_mut() {
                            let sub_inputs = self.sub_block(interner, sub, ix, &mut branch_block.constraints, &mut next_ancestors);
                            inputs.extend(sub_inputs);
                        }
                    }
                    all_inputs.extend(inputs);
                    all_inputs.extend(branch_block.required_fields.iter());
                }
                // get related constraints for all the inputs
                let related = get_input_constraints(&all_inputs, ancestor_constraints);
                let block_name = cur_block.block_name.to_string();
                let if_id = interner.string(&format!("{}|sub_block|if|{}", block_name, ix));

                // add an intermediate scan to the parent for the results of the branches
                let mut parent_if_key = vec![if_id];
                parent_if_key.extend(all_inputs.iter());
                parent_constraints.push(make_intermediate_scan(parent_if_key, output_registers.clone()));

                // fix up the blocks for each branch
                let num_branches = cur_block.sub_blocks.len();
                let branch_ids:Vec<Field> = (0..num_branches).map(|branch_ix| {
                    interner.string(&format!("{}|sub_block|if|{}|branch|{}", block_name, ix, branch_ix))
                }).collect();
                for (branch_ix, sub) in cur_block.sub_blocks.iter_mut().enumerate() {
                    if let &mut SubBlock::IfBranch(ref mut branch_block, ref output_fields) = sub {
                        // add the related constraints to each branch
                        branch_block.constraints.extend(related.iter().map(|v| v.clone()));
                        if exclusive {
                            // Add an intermediate
                            if branch_ix + 1 < num_branches {
                                let mut branch_key = vec![branch_ids[branch_ix]];
                                branch_key.extend(all_inputs.iter());
                                branch_block.constraints.push(make_intermediate_insert(branch_key, vec![], true));
                            }

                            for prev_branch in 0..branch_ix {
                                let mut key_attrs = vec![branch_ids[prev_branch]];
                                key_attrs.extend(all_inputs.iter());
                                branch_block.constraints.push(make_anti_scan(key_attrs));
                            }
                        }
                        let mut if_key = vec![if_id];
                        if_key.extend(all_inputs.iter());
                        branch_block.constraints.push(make_intermediate_insert(if_key, output_fields.clone(), false));
                    }
                }
                all_inputs
            }
        }
    }
}

pub fn get_related_constraints(needles:&Vec<Constraint>, haystack:&Vec<Constraint>) -> (Vec<Constraint>, HashSet<Field>) {
    let mut regs = HashSet::new();
    let mut input_regs = HashSet::new();
    let mut related = needles.clone();
    for needle in needles.iter() {
        for reg in needle.get_registers() {
            regs.insert(reg);
        }
    }
    for hay in haystack {
        let mut found = false;
        let outs = hay.get_output_registers();
        for out in outs.iter() {
            if regs.contains(out) {
                found = true;
                input_regs.insert(*out);
            }
        }
        if found {
            related.push(hay.clone());
        }
    }
    (related, input_regs)
}

pub fn get_input_constraints(needles:&HashSet<Field>, haystack:&Vec<Constraint>) -> Vec<Constraint> {
    let mut related = vec![];
    for hay in haystack {
        let mut found = false;
        let outs = hay.get_output_registers();
        for out in outs.iter() {
            if needles.contains(out) {
                found = true;
            }
        }
        if found {
            related.push(hay.clone());
        }
    }
    related
}

#[derive(Debug, Clone)]
pub struct Compilation {
    block_name: String,
    vars: HashMap<String, usize>,
    var_values: HashMap<Field, Field>,
    unified_registers: HashMap<Field, Field>,
    provided_registers: HashSet<Field>,
    equalities: Vec<(Field, Field)>,
    constraints: Vec<Constraint>,
    sub_blocks: Vec<SubBlock>,
    required_fields: Vec<Field>,
    is_child: bool,
    id: usize,
}

impl Compilation {
    pub fn new(block_name:String) -> Compilation {
        Compilation { vars:HashMap::new(), var_values:HashMap::new(), unified_registers:HashMap::new(), provided_registers:HashSet::new(), equalities:vec![], id:0, block_name, constraints:vec![], sub_blocks:vec![], required_fields:vec![], is_child: false }
    }

    pub fn new_child(parent:&Compilation) -> Compilation {
        let mut child = Compilation::new(parent.block_name.to_string());
        child.id = parent.id + 10000;
        child.is_child = true;
        child
    }

    pub fn get_register(&mut self, name: &str) -> Field {
        let ref mut id = self.id;
        let ix = *self.vars.entry(name.to_string()).or_insert_with(|| { *id += 1; *id });
        register(ix)
    }

    pub fn get_unified_register(&mut self, name: &str) -> Field {
        let reg = self.get_register(name);
        match self.unified_registers.get(&reg) {
            Some(&Field::Register(cur)) => Field::Register(cur),
            _ => reg.clone()
        }
    }

    pub fn get_inputs(&self, haystack: &Vec<Constraint>) -> HashSet<Field> {
        let mut regs = HashSet::new();
        let mut input_regs = HashSet::new();
        for needle in self.constraints.iter() {
            for reg in needle.get_registers() {
                regs.insert(reg);
            }
        }
        regs.extend(self.required_fields.iter());
        for hay in haystack {
            for out in hay.get_output_registers() {
                if regs.contains(&out) {
                    input_regs.insert(out);
                }
            }
        }
        input_regs
    }

    pub fn reassign_registers(&mut self) {
        let mut regs = HashMap::new();
        let ref var_values = self.var_values;
        let mut ix = 0;
        for c in self.constraints.iter() {
            for reg in c.get_registers() {
                regs.entry(reg).or_insert_with(|| {
                    match var_values.get(&reg) {
                        Some(field @ &Field::Value(_)) => field.clone(),
                        _ => {
                            let out = Field::Register(ix);
                            ix += 1;
                            out
                        }
                    }
                });
            }
        }
        for c in self.constraints.iter_mut() {
            c.replace_registers(&regs);
        }
    }

    pub fn get_value(&mut self, name: &str) -> Field {
        let reg = self.get_register(name);
        let val = self.var_values.entry(reg).or_insert(reg);
        val.clone()
    }

    pub fn provide(&mut self, reg:Field) {
        self.provided_registers.insert(reg);
    }

    pub fn is_provided(&mut self, name:&str) -> bool {
        let reg = self.get_register(name);
        self.provided_registers.contains(&reg)
    }
}


named!(pub space, eat_separator!(&b" \t\n\r,"[..]));

#[macro_export]
macro_rules! sp (
  ($i:expr, $($args:tt)*) => (
    {
      sep!($i, space, $($args)*)
    }
  )
);

named!(identifier<&str>, map_res!(is_not_s!("#\\.,()[]{}:=\"|; \r\n\t"), str::from_utf8));

named!(number<Node<'a>>,
       alt_complete!(
           recognize!(delimited!(digit, tag!("."), digit)) => { |v:&[u8]| {
               let s = str::from_utf8(v).unwrap();
               Node::Float(f32::from_str(s).unwrap())
           }} |
           recognize!(digit) => {|v:&[u8]| {
               let s = str::from_utf8(v).unwrap();
               Node::Integer(i32::from_str(s).unwrap())
           }}));

named!(raw_string<&str>,
       delimited!(
           tag!("\""),
           map_res!(escaped!(is_not_s!("\"\\"), '\\', one_of!("\"\\")), str::from_utf8),
           tag!("\"")
       ));

named!(string_embed<Node<'a>>,
       delimited!(
           tag!("{{"),
           expr,
           tag!("}}")
       ));

// @FIXME: seems like there should be a better way to handle this
named!(not_embed_start<&[u8]>, is_not_s!("{"));
named!(string_parts<Vec<Node<'a>>>,
       fold_many1!(
           alt_complete!(
               string_embed |
               map_res!(not_embed_start, str::from_utf8) => { |v:&'a str| Node::RawString(v) } |
               map_res!(recognize!(pair!(tag!("{"), not_embed_start)), str::from_utf8) => { |v:&'a str| Node::RawString(v) }),
           Vec::new(),
           |mut acc: Vec<Node<'a>>, cur: Node<'a>| {
               acc.push(cur);
               acc
           }));

named!(string<Node<'a>>,
       do_parse!(
           raw: raw_string >>
           ({
               let info = string_parts(raw.as_bytes());
               let mut parts = info.unwrap().1;
               match (parts.len(), parts.get(0)) {
                   (1, Some(&Node::RawString(_))) => parts.pop().unwrap(),
                   _ => Node::EmbeddedString(None, parts)
               }
           })));

named!(variable<Node<'a>>,
       do_parse!(i: identifier >>
                 (Node::Variable(i))));

named!(value<Node<'a>>,
       sp!(alt_complete!(
               number |
               string |
               record_function |
               record_reference |
               delimited!(tag!("("), expr, tag!(")"))
               )));

named!(expr<Node<'a>>,
       sp!(alt_complete!(
               infix_addition |
               infix_multiplication |
               value
               )));

named!(expr_set<Node<'a>>,
       do_parse!(
           items: sp!(delimited!(tag!("("), many1!(sp!(expr)) ,tag!(")"))) >>
           (Node::ExprSet(items))));

named!(hashtag<Node>,
       do_parse!(
           tag!("#") >>
           tag_name: identifier >>
           (Node::Tag(tag_name))));

named!(attribute_inequality<Node<'a>>,
       do_parse!(
           attribute: identifier >>
           op: sp!(alt_complete!(tag!(">=") | tag!("<=") | tag!("!=") | tag!("<") | tag!(">") | tag!("contains") | tag!("!contains"))) >>
           right: expr >>
           (Node::AttributeInequality{attribute, right:Box::new(right), op:str::from_utf8(op).unwrap()})));

named!(record_set<Node<'a>>,
       do_parse!(
           records: many1!(sp!(record)) >>
           (Node::RecordSet(records))));

named!(attribute_equality<Node<'a>>,
       do_parse!(
           attr: identifier >>
           sp!(alt_complete!(tag!(":") | tag!("="))) >>
           value: alt_complete!(record_set | expr | expr_set) >>
           (Node::AttributeEquality(attr, Box::new(value)))));

named!(attribute<Node<'a>>,
       sp!(alt_complete!(
               hashtag |
               attribute_equality |
               attribute_inequality |
               identifier => { |v:&'a str| Node::Attribute(v) })));

named!(record<Node<'a>>,
       do_parse!(
           tag!("[") >>
           attrs: many0!(attribute) >>
           tag!("]") >>
           (Node::Record(None, attrs))));

named!(inequality<Node<'a>>,
       do_parse!(
           left: expr >>
           op: sp!(alt_complete!(tag!(">=") | tag!("<=") | tag!("!=") | tag!("<") | tag!(">") | tag!("contains") | tag!("!contains"))) >>
           right: expr >>
           (Node::Inequality{left:Box::new(left), right:Box::new(right), op:str::from_utf8(op).unwrap()})));

named!(infix_addition<Node<'a>>,
       do_parse!(
           left: alt_complete!(infix_multiplication | value) >>
           op: sp!(alt_complete!(tag!("+") | tag!("-"))) >>
           right: expr >>
           (Node::Infix{result:None, left:Box::new(left), right:Box::new(right), op:str::from_utf8(op).unwrap()})));

named!(infix_multiplication<Node<'a>>,
       do_parse!(
           left: value >>
           op: sp!(alt_complete!(tag!("*") | tag!("/"))) >>
           right: alt_complete!(infix_multiplication | value) >>
           (Node::Infix{result:None, left:Box::new(left), right:Box::new(right), op:str::from_utf8(op).unwrap()})));

named!(record_function<Node<'a>>,
       do_parse!(
          op: identifier >>
          tag!("[") >>
          params: sp!(many0!(alt_complete!(
                    attribute_equality |
                    identifier => { |v:&'a str| Node::Attribute(v) }))) >>
          tag!("]") >>
          (Node::RecordFunction { op, params, outputs:vec![]})));

named!(multi_function_equality<Node<'a>>,
       do_parse!(
           outputs: alt_complete!(variable => { |v| vec![v] } |
                                  delimited!(tag!("("), many1!(sp!(variable)), tag!(")"))) >>
           sp!(tag!("=")) >>
           func: record_function >>
           ({
               if let Node::RecordFunction { op, params, .. } = func {
                   Node::RecordFunction { outputs, op, params}
               } else {
                   panic!("Non function return from record_function parser")
               }
           })));

named!(equality<Node<'a>>,
       do_parse!(
           left: expr >>
           op: sp!(tag!("=")) >>
           right: alt_complete!(expr | record) >>
           (Node::Equality {left:Box::new(left), right:Box::new(right)})));

named_args!(output_record_set<'a>(output_type:OutputType) <Node<'this_is_probably_unique_i_hope_please>>,
       do_parse!(
           records: many1!(sp!(apply!(output_record, output_type))) >>
           (Node::RecordSet(records))));

named_args!(output_attribute_equality<'a>(output_type:OutputType) <Node<'this_is_probably_unique_i_hope_please>>,
       do_parse!(
           attr: identifier >>
           sp!(alt_complete!(tag!(":") | tag!("="))) >>
           value: alt_complete!(apply!(output_record_set, output_type) | expr | expr_set) >>
           (Node::AttributeEquality(attr, Box::new(value)))));

named_args!(output_attribute<'a>(output_type:OutputType) <Node<'this_is_probably_unique_i_hope_please>>,
       sp!(alt_complete!(
               hashtag |
               apply!(output_attribute_equality, output_type) |
               tag!("|") => { |_| Node::Pipe } |
               identifier => { |v:&'this_is_probably_unique_i_hope_please str| Node::Attribute(v) })));

named_args!(output_record<'a>(output_type:OutputType) <Node<'this_is_probably_unique_i_hope_please>>,
       do_parse!(
           tag!("[") >>
           attrs: many0!(apply!(output_attribute, output_type)) >>
           tag!("]") >>
           (Node::OutputRecord(None, attrs, output_type))));

named!(attribute_access<Node<'a>>,
       do_parse!(start: identifier >>
                 rest: many1!(pair!(tag!("."), identifier)) >>
                 ({
                     let mut items = vec![start];
                     for (_, v) in rest {
                         items.push(v);
                     }
                     Node::AttributeAccess(items)
                 })));

named!(record_reference<Node<'a>>,
       sp!(alt_complete!(attribute_access | variable)));

named!(mutating_attribute_access<Node<'a>>,
       do_parse!(start: identifier >>
                 rest: many1!(pair!(tag!("."), identifier)) >>
                 ({
                     let mut items = vec![start];
                     for (_, v) in rest {
                         items.push(v);
                     }
                     Node::MutatingAttributeAccess(items)
                 })));

named!(mutating_record_reference<Node<'a>>,
       sp!(alt_complete!(mutating_attribute_access | variable)));

named!(bind_update<Node<'a>>,
       do_parse!(
           record: mutating_record_reference >>
           op: sp!(alt_complete!(tag!("+=") | tag!("<-"))) >>
           value: alt_complete!(apply!(output_record, OutputType::Bind) | expr | hashtag) >>
           (Node::RecordUpdate{ record: Box::new(record), op: str::from_utf8(op).unwrap(), value: Box::new(value), output_type:OutputType::Bind })));

named!(none_value<Node<'a>>,
       do_parse!(
           tag!("none") >>
           ( Node::NoneValue )));

named!(commit_update<Node<'a>>,
       do_parse!(
           record: mutating_record_reference >>
           op: sp!(alt_complete!(tag!(":=") | tag!("+=") | tag!("-=") | tag!("<-"))) >>
           value: alt_complete!(apply!(output_record, OutputType::Commit) | none_value | expr | hashtag) >>
           (Node::RecordUpdate{ record: Box::new(record), op: str::from_utf8(op).unwrap(), value: Box::new(value), output_type:OutputType::Commit })));

named_args!(output_equality<'a>(output_type:OutputType) <Node<'this_is_probably_unique_i_hope_please>>,
       do_parse!(
           left: identifier >>
           sp!(tag!("=")) >>
           right: apply!(output_record, output_type) >>
           (Node::Equality {left:Box::new(Node::Variable(left)), right:Box::new(right)})));

named!(not_form<Node<'a>>,
       do_parse!(
           sp!(tag!("not")) >>
           items: delimited!(tag!("("),
                             many0!(sp!(alt_complete!(
                                         multi_function_equality |
                                         inequality |
                                         record |
                                         equality
                                         ))),
                             tag!(")")) >>
           (Node::Not(0, items))));

named!(if_equality<Vec<Node<'a>>>,
       do_parse!(
           outputs: alt_complete!(expr => { |v| vec![v] } |
                                  delimited!(tag!("("), many1!(sp!(expr)), tag!(")"))) >>
           sp!(tag!("=")) >>
           (outputs)));

named!(if_else_branch<Node<'a>>,
       alt_complete!(
           if_branch |
           do_parse!(
               sp!(tag!("else")) >>
               branch: if_branch >>
               ({
                   if let Node::IfBranch { ref mut exclusive, .. } = branch.clone() {
                       *exclusive = true;
                       branch
                   } else {
                       panic!("Invalid if branch");
                   }
               })) |
           do_parse!(
               sp!(tag!("else")) >>
               result: alt_complete!(expr | expr_set) >>
               (Node::IfBranch {sub_block_id:0, exclusive:true, body:vec![], result:Box::new(result)}))));

named!(if_branch<Node<'a>>,
       do_parse!(
           sp!(tag!("if")) >>
           body: many0!(sp!(alt_complete!(
                       multi_function_equality |
                       not_form |
                       inequality |
                       record |
                       equality
                       ))) >>
           sp!(tag!("then")) >>
           result: alt_complete!(expr | expr_set) >>
           (Node::IfBranch {sub_block_id:0, exclusive:false, body, result:Box::new(result)})
                ));

named!(if_expression<Node<'a>>,
       do_parse!(
           outputs: opt!(if_equality) >>
           start_branch: if_branch >>
           other_branches: many0!(if_else_branch) >>
           ({
               let exclusive = other_branches.iter().any(|b| {
                   if let &Node::IfBranch {exclusive, ..} = b {
                       exclusive
                   } else {
                       false
                   }
               });
               let mut branches = vec![start_branch];
               branches.extend(other_branches);
               Node::If {sub_block_id:0, exclusive, outputs, branches}
           })));


named!(search_section<Node<'a>>,
       do_parse!(
           sp!(tag!("search")) >>
           items: many0!(sp!(alt_complete!(
                            not_form |
                            multi_function_equality |
                            if_expression |
                            inequality |
                            record |
                            equality
                        ))) >>
           (Node::Search(items))));

named!(bind_section<Node<'a>>,
       do_parse!(
           sp!(tag!("bind")) >>
           items: many1!(sp!(alt_complete!(
                       apply!(output_equality, OutputType::Bind) |
                       apply!(output_record, OutputType::Bind) |
                       complete!(bind_update)
                       ))) >>
           (Node::Bind(items))));

named!(commit_section<Node<'a>>,
       do_parse!(
           sp!(tag!("commit")) >>
           items: many1!(sp!(alt_complete!(
                       apply!(output_equality, OutputType::Commit) |
                       apply!(output_record, OutputType::Commit) |
                       complete!(commit_update)
                       ))) >>
           (Node::Commit(items))));

named!(project_section<Node<'a>>,
       do_parse!(
           sp!(tag!("project")) >>
           items: sp!(delimited!(tag!("("), many1!(sp!(expr)) ,tag!(")"))) >>
           (Node::Project(items))));

named!(watch_section<Node<'a>>,
       do_parse!(
           sp!(tag!("watch")) >>
           watcher: sp!(identifier) >>
           items: sp!(delimited!(tag!("("), many1!(sp!(expr)) ,tag!(")"))) >>
           (Node::Watch(watcher, items))));

named!(block<Node<'a>>,
       sp!(do_parse!(
               search: opt!(search_section) >>
               update: alt_complete!( bind_section | commit_section | project_section | watch_section ) >>
               sp!(tag!("end")) >>
               (Node::Block {search:Box::new(search), update:Box::new(update)}))));

named!(maybe_block<Option<Node<'a>>>,
       alt_complete!(block => { |block| Some(block) } |
                     eof!() => { |_| None }));

named!(surrounded_block<Option<Node<'a>>>,
       do_parse!(
           res: many_till!(anychar, maybe_block) >>
           (res.1)));

named!(markdown<Node<'a>>,
       sp!(do_parse!(
               maybe_blocks: many1!(surrounded_block) >>
               ({
                   let mut blocks = vec![];
                   for block in maybe_blocks {
                       if let Some(v) = block {
                           blocks.push(v.clone());
                       }
                   }
                   Node::Doc { file:"foo.eve".to_string(), blocks}
               }))));

pub fn make_block(interner:&mut Interner, name:&str, content:&str) -> Vec<Block> {
    let mut blocks = vec![];
    let parsed = block(content.as_bytes());
    let mut comp = Compilation::new(name.to_string());
    // println!("Parsed {:?}", parsed);
    match parsed {
        IResult::Done(_, mut block) => {
            block.gather_equalities(interner, &mut comp);
            block.unify(&mut comp);
            block.compile(interner, &mut comp);
        }
        _ => { println!("Failed: {:?}", parsed); }
    }

    comp.reassign_registers();
    for c in comp.constraints.iter() {
        println!("{:?}", c);
    }
    let sub_ix = 0;
    let mut subs = comp.sub_blocks.clone();
    while subs.len() > 0 {
        let cur = subs.pop().unwrap();
        let mut sub_comp = match cur {
            SubBlock::Not(comp) => comp,
            SubBlock::IfBranch(comp,..) => comp,
            SubBlock::If(comp,..) => comp,
        };
        if sub_comp.constraints.len() > 0 {
            sub_comp.reassign_registers();
            println!("    SubBlock");
            for c in sub_comp.constraints.iter() {
                println!("        {:?}", c);
            }
            blocks.push(Block::new(&format!("block|{}|sub_block|{}", name, sub_ix), sub_comp.constraints));
        }
        subs.extend(sub_comp.sub_blocks);
    }

    blocks.push(Block::new(name, comp.constraints));
    blocks
}

pub fn parse_string(program:&mut Program, content:&str, path:&str) -> Vec<Block> {
    let res = markdown(content.as_bytes());
    if let IResult::Done(_, mut cur) = res {
        if let Node::Doc { ref mut blocks, .. } = cur {
            let interner = &mut program.state.interner;
            let mut program_blocks = vec![];
            let mut ix = 0;
            for block in blocks {
                // println!("\n\nBLOCK!");
                // println!("  {:?}\n", block);
                ix += 1;
                let block_name = format!("{}|block|{}", path, ix);
                let mut comp = Compilation::new(block_name.to_string());
                block.gather_equalities(interner, &mut comp);
                block.unify(&mut comp);
                block.compile(interner, &mut comp);
                comp.reassign_registers();
                println!("Block");
                for c in comp.constraints.iter() {
                    println!("{:?}", c);
                }
                let sub_ix = 0;
                let mut subs = comp.sub_blocks.clone();
                while subs.len() > 0 {
                    let cur = subs.pop().unwrap();
                    let mut sub_comp = match cur {
                        SubBlock::Not(comp) => comp,
                        SubBlock::IfBranch(comp,..) => comp,
                        SubBlock::If(comp,..) => comp,
                    };
                    if sub_comp.constraints.len() > 0 {
                        sub_comp.reassign_registers();
                        println!("    SubBlock");
                        for c in sub_comp.constraints.iter() {
                            println!("        {:?}", c);
                        }
                        program_blocks.push(Block::new(&format!("{}|sub_block|{}", block_name, sub_ix), sub_comp.constraints));
                    }
                    subs.extend(sub_comp.sub_blocks);
                }
                println!("");
                program_blocks.push(Block::new(&block_name, comp.constraints));
            }
            program_blocks
        } else {
            panic!("Got a non-doc parse??");
        }
    } else if let IResult::Error(Err::Position(err, pos)) = res {
        println!("ERROR: {:?}", err.description());
        println!("{:?}", str::from_utf8(pos));
        panic!("Failed to parse");
    } else {
        panic!("Failed to parse");
    }
}

pub fn parse_file(program:&mut Program, path:&str) -> Vec<Block> {
    let mut file = File::open(path).expect("Unable to open the file");
    let mut contents = String::new();
    file.read_to_string(&mut contents).expect("Unable to read the file");
    parse_string(program, &contents, path)
}

#[test]
pub fn parser_test() {
    let mut file = File::open("examples/test2.eve").expect("Unable to open the file");
    let mut contents = String::new();
    file.read_to_string(&mut contents).expect("Unable to read the file");
    let x = markdown(contents.as_bytes());
    println!("{:?}", x);
}

