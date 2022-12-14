/*
    This file is part of Ever OS.

    Ever OS is free software: you can redistribute it and/or modify
    it under the terms of the Apache License 2.0 (http://www.apache.org/licenses/)

    Copyright 2019-2021 (c) TON LABS
*/

/*

    TODO:
        - move [pyfunction] code to separate file

*/

extern crate base64;
extern crate ed25519_dalek;
extern crate hex;
#[macro_use]
extern crate lazy_static;
extern crate num;
extern crate rand;
// #[macro_use]
extern crate serde_json;

extern crate ton_block;
extern crate ton_types;
#[macro_use]
extern crate ton_vm;
extern crate ton_abi;

use serde_json::Value as JsonValue;

use ed25519_dalek::{
    Keypair, Signer,
};

use rand::rngs::OsRng;
use rand::rngs::StdRng;
use rand::SeedableRng;

use pyo3::prelude::*;
use pyo3::wrap_pyfunction;
use pyo3::exceptions::PyRuntimeError;

use std::io::Cursor;
use std::io::Write;

use ton_block::{
    Serializable, CurrencyCollection,
};

use ton_types::{
    SliceData, Cell,
    serialize_toc,
    cells_serialization::{deserialize_cells_tree},
    BagOfCells
};

mod printer;
mod util;
mod abi;
mod actions;
mod debots;
mod debug_info;
mod global_state;
mod exec;
mod call_contract;
mod messages;

use global_state::{
    GlobalState, GlobalConfig, GLOBAL_STATE,
};

use util::{
    decode_address, load_from_file,
};

use messages::{
    CallContractMsgInfo,
    MsgInfo,
};

use debots::{
    build_internal_message,
};

use exec::{
    exec_contract_and_process_actions,
    generate_contract_address,
    dispatch_message_impl,
    deploy_contract_impl,
    call_contract_impl,
    load_state_init,
    encode_message_body_impl,
    decode_message,
};

#[pyfunction]
fn trace_on() -> PyResult<()> {
    GLOBAL_STATE.lock().unwrap().trace_on = true;
    Ok(())
}

#[pyfunction]
fn set_debot_keypair(secret: Option<String>, pubkey: Option<String>) -> PyResult<()> {
    let keypair = if secret.is_none() {
        None
    } else {
        Some(ton_client::crypto::KeyPair::new(pubkey.unwrap(), secret.unwrap()))
    };
    GLOBAL_STATE.lock().unwrap().debot_keypair = keypair;
    Ok(())
}

#[pyfunction]
fn get_global_config() -> PyResult<GlobalConfig> {
    let config = GLOBAL_STATE.lock().unwrap().config.clone();
    Ok(config)
}

#[pyfunction]
fn set_global_config(cfg: GlobalConfig) -> PyResult<()> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    gs.config = cfg;
    Ok(())
}

#[pyfunction]
fn gen_addr(
    contract_file: String,
    abi_file: String,
    initial_data: Option<String>,
    pubkey: Option<String>,
    private_key: Option<String>,
    wc: i8
) -> PyResult<String> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    // let trace = gs.is_trace(1);

    let abi_info = gs.all_abis.from_file(&abi_file)
                     .map_err(|e| PyRuntimeError::new_err(e))?;

    let state_init = load_state_init(
        &mut gs,
        &contract_file,
        &abi_file,
        &abi_info,
        &None,  // ctor_params
        &initial_data,
        &pubkey,
        &private_key,
    ).map_err(|e| PyRuntimeError::new_err(e))?;
    
    let addr = generate_contract_address(&state_init, wc);
    let addr_str = format!("{}", addr);

    Ok(addr_str)
}

#[pyfunction]
fn deploy_contract(
    contract_file: String,
    abi_file: String,
    ctor_params: Option<String>,
    initial_data: Option<String>,
    pubkey: Option<String>,
    private_key: Option<String>,
    wc: i8,
    override_address: Option<String>,
    balance: u64,
) -> PyResult<String> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    // let trace = gs.is_trace(1);

    let abi_info = gs.all_abis.from_file(&abi_file)
                     .map_err(|e| PyRuntimeError::new_err(e))?;

    let state_init = load_state_init(
        &mut gs,
        &contract_file,
        &abi_file,
        &abi_info,
        &ctor_params,
        &initial_data,
        &pubkey,
        &private_key,
    ).map_err(|e| PyRuntimeError::new_err(e))?;

    let target_address = override_address.map(|addr| decode_address(&addr));
    deploy_contract_impl(
        &mut gs,
        Some(contract_file),
        state_init,
        target_address,
        abi_info,
        wc,
        balance,
    ).map_err(|err_str| PyRuntimeError::new_err(err_str))
}

#[pyfunction]
fn fetch_contract_state(address: String) -> PyResult<(Option<String>, Option<String>)> {
    let address = decode_address(&address);
    let gs = GLOBAL_STATE.lock().unwrap();
    let contract = gs.get_contract(&address);
    if contract.is_none() {
        return Ok((None, None))
    }
    let contract = contract.unwrap();
    let state_init = contract.state_init();

    let code = state_init.code.as_ref().unwrap();
    let code = serialize_toc(&code).unwrap();
    let code = base64::encode(&code);
    let data = state_init.data.as_ref().unwrap();
    let data = serialize_toc(&data).unwrap();
    let data = base64::encode(&data);

    Ok((Some(code), Some(data)))
}

#[pyfunction]
fn save_tvc(address: String, filename: String) -> PyResult<()> {
    let address = decode_address(&address);
    let gs = GLOBAL_STATE.lock().unwrap();
    let contract = gs.get_contract(&address).unwrap();

    let root = contract.state_init().write_to_new_cell()
        .map_err(|e| format!("Serialization failed: {}", e)).unwrap()
        .into();
    let mut buffer = vec![];
    BagOfCells::with_root(&root).write_to(&mut buffer, false)
        .map_err(|e| format!("BOC failed: {}", e)).unwrap();

    let mut file = std::fs::File::create(&filename).unwrap();
    file.write_all(&buffer).map_err(|e| format!("Write to file failed: {}", e)).unwrap();

    Ok(())
}

#[pyfunction]
fn get_balance(address: String) -> PyResult<Option<u64>> {
    let address = decode_address(&address);
    let gs = GLOBAL_STATE.lock().unwrap();
    let contract = gs.get_contract(&address);
    let balance = if gs.dummy_balances.contains_key(&address) {
        assert!(contract.is_none());
        Some(gs.dummy_balances[&address])
    } else {
        contract.map(|c| c.balance())
    };
    Ok(balance)
}

#[pyfunction]
fn set_balance(address: String, balance: u64) -> PyResult<()> {
    let address = decode_address(&address);
    let mut gs = GLOBAL_STATE.lock().unwrap();
    let mut contract_info = gs.get_contract(&address).unwrap();
    contract_info.set_balance(balance);
    gs.set_contract(address, contract_info);
    Ok(())
}

#[pyfunction]
fn dispatch_message(msg_id: u32) -> PyResult<String> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    let result = dispatch_message_impl(&mut gs, msg_id);
    gs.last_trace = result.trace.clone();
    Ok(result.to_string())
}

#[pyfunction]
fn set_contract_abi(address_str: Option<String>, abi_file: String) -> PyResult<()> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    let abi_info = gs.all_abis.from_file(&abi_file)
                     .map_err(|e| PyRuntimeError::new_err(e))?;
    if let Some(address_str) = address_str {
        let addr = decode_address(&address_str);
        let contract_info = gs.get_contract(&addr);
        if contract_info.is_none() {
            let err = format!("Unable to set ABI for non-existent address {}", addr);
            return Err(PyRuntimeError::new_err(err));
        }
        let mut contract_info = contract_info.unwrap();
        contract_info.set_abi(abi_info);
        gs.set_contract(addr, contract_info);
    }
    Ok(())
}

#[pyfunction]
fn call_ticktock(
    address_str: String,
    is_tock: bool,
) -> PyResult<String> {
    let address = decode_address(&address_str);

    let mut gs = GLOBAL_STATE.lock().unwrap();
    // TODO: move to call_ticktock_impl()
    let msg_info = CallContractMsgInfo::with_ticktock(is_tock, address.clone());

    let result = exec_contract_and_process_actions(
        &mut gs,
        &msg_info,
        None, // method
        false, // is_debot_call
    );

    // TODO: register in gs.messages?

    Ok(result.to_string())
}

#[pyfunction]
fn log_str(
    msg: String,
) -> PyResult<()> {

    let mut gs = GLOBAL_STATE.lock().unwrap();
    gs.log_str(msg);

    Ok(())
}

#[pyfunction]
fn call_contract(                   // TODO: is this message added to message store?
    address_str: String,
    method: String,
    is_getter: bool,
    is_debot: bool,
    params: String,
    private_key: Option<String>,
) -> PyResult<String> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    let result =
        call_contract_impl(&mut gs, address_str, method,
                           is_getter, is_debot, params, private_key);
    if let Ok(ref result) = result {
        gs.last_trace = result.trace.clone();
    }
    let result = result.map_err(|e| PyRuntimeError::new_err(e))?;
    Ok(result.to_string())
}

// ---------------------------------------------------------------------------------------

#[pyfunction]
fn set_now(now: u64) -> PyResult<()> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    gs.set_now(now);
    Ok(())
}

#[pyfunction]
fn get_now() -> PyResult<u64> {
    let gs = GLOBAL_STATE.lock().unwrap();
    let result = gs.get_now();

    Ok(result)
}

#[pyfunction]
fn set_config_param(idx: u32, cell: String) -> PyResult<()> {
    let mut gs = GLOBAL_STATE.lock().unwrap();

    let cell = decode_cell(cell);

    let is_empty = cell.bit_length() == 0;
    if gs.is_trace(1) {
        println!("set_config_param {} is_empty={}", idx, is_empty);
    }
    if is_empty {
        gs.config_params.remove(&idx);
    } else {
        gs.config_params.insert(idx, cell);
    }

    Ok(())
}

#[pyfunction]
fn reset_all() -> PyResult<()> {
    use std::ops::DerefMut;
    let mut gs = GLOBAL_STATE.lock().unwrap();
    *gs.deref_mut() = GlobalState::default();
    Ok(())
}

#[pyfunction]
fn make_keypair(seed : Option<u64>) -> PyResult<(String, String)> {
    let keypair = match seed {
        Some(seed) => {
            let mut csprng = StdRng::seed_from_u64(seed);
            Keypair::generate(&mut csprng)
        },
        None => {
            let mut csprng = OsRng{};
            Keypair::generate(&mut csprng)
        }
    };
    let secret = keypair.to_bytes();
    let secret = hex::encode(secret.to_vec());
    let public = hex::encode(keypair.public.to_bytes());
    Ok((secret, public))
}

#[pyfunction]
fn sign_cell(cell: String, secret: String) -> PyResult<String> {
    let cell = decode_cell(cell);

    let secret = hex::decode(secret).unwrap();
    let keypair = Keypair::from_bytes(&secret).expect("error: invalid key");

    let data = SliceData::from(cell).get_bytestring(0);
    let signature = keypair.sign(&data).to_bytes();
    let signature = hex::encode(signature.to_vec());

    Ok(signature)
}

#[pyfunction]
fn get_all_runs() -> PyResult<String> {
    let gs = GLOBAL_STATE.lock().unwrap();
    let result = serde_json::to_string(&gs.runs).unwrap();
    Ok(result)
}

#[pyfunction]
fn get_all_messages() -> PyResult<String> {
    let gs = GLOBAL_STATE.lock().unwrap();
    let jsons: JsonValue = gs.messages.to_json();
    let result = serde_json::to_string(&jsons).unwrap();
    Ok(result)
}

#[pyfunction]
fn get_last_trace() -> PyResult<String> {
    let gs = GLOBAL_STATE.lock().unwrap();
    let result = serde_json::to_string(&gs.last_trace).unwrap();
    Ok(result)
}

#[pyfunction]
fn get_last_error_msg() -> PyResult<Option<String>> {
    let gs = GLOBAL_STATE.lock().unwrap();
    Ok(gs.last_error_msg.clone())
}

fn dump_cell_rec(cell: Cell, pfx: String) {
    let slice: SliceData = cell.into();
    println!("{}> {:x}", pfx, slice);
    let n = slice.remaining_references();
    let pfx = pfx + "  ";
    for i in 0..n {
        dump_cell_rec(slice.reference(i).unwrap(), pfx.clone());
    }
}

#[pyfunction]
fn dump_cell(cell: String) -> PyResult<()> {
    let cell = decode_cell(cell);
    // println!("cell = {:?}", cell);
    // println!("cell = {}", cell);
    // println!("cell = {:x}", cell);
    // let slice: SliceData = cell.clone().into();

    // println!("slice = {:?}", slice);
    // println!("slice = {}", slice);
    // println!("slice = {:x}", slice);

    dump_cell_rec(cell, "".to_string());

    Ok(())
}

#[pyfunction]
fn load_code_cell(filename: String) -> PyResult<String> {
    let state_init = load_from_file(&filename).map_err(|e| PyRuntimeError::new_err(e))?;
    let code = state_init.code.unwrap();
    let bytes = serialize_toc(&code).unwrap();
    Ok(base64::encode(&bytes))
}

#[pyfunction]
fn load_data_cell(filename: String) -> PyResult<String> {
    // TODO: add tests for that
    let state_init = load_from_file(&filename).map_err(|e| PyRuntimeError::new_err(e))?;
    let data = state_init.data.unwrap();
    let bytes = serialize_toc(&data).unwrap();
    Ok(base64::encode(&bytes))
}

fn decode_cell(cell: String) -> Cell {
    let cell = base64::decode(&cell).unwrap();
    let mut csor = Cursor::new(cell);
    let cell = deserialize_cells_tree(&mut csor).unwrap().remove(0);
    cell    
}

#[pyfunction]
fn get_compiler_version_from_cell(cell: String) -> PyResult<Option<String>> {
    let cell = decode_cell(cell);
    let result = ton_client::boc::get_compiler_version_from_cell(cell).unwrap();
    return Ok(result);
}

#[pyfunction]
fn get_cell_repr_hash(cell: String) -> PyResult<String> {
    // TODO: make CellWrapper class interoperable with Python
    let cell = decode_cell(cell);
    let hash = cell.repr_hash().as_hex_string();
    Ok(hash)
}


#[pyfunction]
fn encode_message_body(abi_file: String, method: String, params: String) -> PyResult<String> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    let abi_info = gs.all_abis
        .from_file(&abi_file)
        .map_err(|e| PyRuntimeError::new_err(e))?;
    let cell = encode_message_body_impl(&abi_info, method, params);
    let result = serialize_toc(&cell.unwrap()).unwrap();
    Ok(base64::encode(&result))
}

#[pyfunction]
fn debot_translate_getter_answer(msg_id: u32) -> PyResult<String> {
    let mut gs = GLOBAL_STATE.lock().unwrap();
    let msg_info = debots::debot_translate_getter_answer_impl(&mut gs, msg_id)
        .map_err(|e| PyRuntimeError::new_err(e))?;
    Ok(msg_info.json_str())
}

#[pyfunction]
fn build_int_msg(src: String, dst: String, body: String, value: u64) -> PyResult<String> {
    let mut gs = GLOBAL_STATE.lock().unwrap();

    let src = decode_address(&src);
    let dst = decode_address(&dst);

    let contract = gs.get_contract(&dst).unwrap();

    let cell = decode_cell(body);

    let msg = build_internal_message(src, dst, cell.into(), CurrencyCollection::with_grams(value));

    let j = decode_message(&gs, contract.abi_info(), None, &msg, 0, false);
    let msg_info = MsgInfo::create(msg.clone(), j);
    let msg_info = gs.messages.add(msg_info);

    Ok(msg_info.json_str())
}


/////////////////////////////////////////////////////////////////////////////////////
/// A Python module implemented in Rust.
#[pymodule]
fn linker_lib(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_wrapped(wrap_pyfunction!(reset_all))?;

    m.add_wrapped(wrap_pyfunction!(deploy_contract))?;
    m.add_wrapped(wrap_pyfunction!(gen_addr))?;
    m.add_wrapped(wrap_pyfunction!(call_contract))?;
    m.add_wrapped(wrap_pyfunction!(call_ticktock))?;
    m.add_wrapped(wrap_pyfunction!(log_str))?;
    m.add_wrapped(wrap_pyfunction!(get_balance))?;
    m.add_wrapped(wrap_pyfunction!(set_balance))?;
    m.add_wrapped(wrap_pyfunction!(fetch_contract_state))?;

    m.add_wrapped(wrap_pyfunction!(dispatch_message))?;
    m.add_wrapped(wrap_pyfunction!(debot_translate_getter_answer))?;
    m.add_wrapped(wrap_pyfunction!(build_int_msg))?;

    m.add_wrapped(wrap_pyfunction!(get_global_config))?;
    m.add_wrapped(wrap_pyfunction!(set_global_config))?;

    m.add_wrapped(wrap_pyfunction!(set_now))?;
    m.add_wrapped(wrap_pyfunction!(get_now))?;
    m.add_wrapped(wrap_pyfunction!(trace_on))?;
    m.add_wrapped(wrap_pyfunction!(set_contract_abi))?;
    m.add_wrapped(wrap_pyfunction!(set_config_param))?;

    m.add_wrapped(wrap_pyfunction!(make_keypair))?;
    m.add_wrapped(wrap_pyfunction!(sign_cell))?;
    m.add_wrapped(wrap_pyfunction!(load_code_cell))?;
    m.add_wrapped(wrap_pyfunction!(load_data_cell))?;
    m.add_wrapped(wrap_pyfunction!(get_compiler_version_from_cell))?;
    m.add_wrapped(wrap_pyfunction!(get_cell_repr_hash))?;
    m.add_wrapped(wrap_pyfunction!(encode_message_body))?;
    m.add_wrapped(wrap_pyfunction!(dump_cell))?;

    m.add_wrapped(wrap_pyfunction!(get_all_runs))?;
    m.add_wrapped(wrap_pyfunction!(get_all_messages))?;
    m.add_wrapped(wrap_pyfunction!(get_last_trace))?;
    m.add_wrapped(wrap_pyfunction!(get_last_error_msg))?;

    m.add_wrapped(wrap_pyfunction!(set_debot_keypair))?;
    m.add_wrapped(wrap_pyfunction!(save_tvc))?;

    Ok(())
}

