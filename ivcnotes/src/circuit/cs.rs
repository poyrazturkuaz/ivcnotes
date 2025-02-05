use crate::note::NoteOutIndex;
use ark_ff::Field;
use ark_r1cs_std::alloc::AllocVar;
use ark_r1cs_std::boolean::Boolean;
use ark_r1cs_std::eq::EqGadget;
use ark_r1cs_std::fields::fp::FpVar;
use ark_r1cs_std::fields::nonnative::NonNativeFieldVar;
use ark_r1cs_std::select::CondSelectGadget;
use ark_relations::r1cs::{ConstraintSystemRef, Result as CSResult, SynthesisError};

use super::inputs::{witness_in, witness_point_in, NoteVar, PublicInputVar};
use super::{verify_signature, Circuit, IVC};

pub(crate) fn synth<E: IVC>(cs: ConstraintSystemRef<E::Field>, cir: Circuit<E>) -> CSResult<()> {
    let pi = cir.public.as_ref();
    let aux = cir.aux.as_ref();

    let zero = E::Field::ZERO;
    let const_zero = FpVar::new_constant(cs.clone(), zero)?;
    let const_true = Boolean::new_constant(cs.clone(), true)?;

    let index_issue =
        FpVar::new_constant(cs.clone(), (NoteOutIndex::Issue {}).inner::<E::Field>())?;
    let index_0 = FpVar::new_constant(cs.clone(), (NoteOutIndex::Out0 {}).inner::<E::Field>())?;
    let index_1 = FpVar::new_constant(cs.clone(), (NoteOutIndex::Out1 {}).inner::<E::Field>())?;

    let pi = PublicInputVar::new(cs.clone(), pi)?;

    // identity commitment integrity
    let pubkey = witness_point_in(cs.clone(), aux, |e| *e.public_key.as_ref())?;
    let nullifier_key = witness_in(cs.clone(), aux, |e| e.nullifier_key)?;
    let sender = cir
        .h
        .var_id_commitment(cs.clone(), &nullifier_key, &pubkey)?;
    pi.sender.enforce_equal(&sender)?;

    // Branch 1: IssueTx
    let is_issue_tx = pi.step.is_eq(&const_zero)?;
    let (sighash_issue, _note_hash, is_issue_tx) = {
        let value = witness_in(cs.clone(), aux, |e| E::Field::from(e.value_out))?;
        let blind = witness_in(cs.clone(), aux, |e| e.blind_out_0)?;
        let note = NoteVar::new(
            &pi.asset_hash,
            &pi.sender,
            &value,
            &pi.step,
            &const_zero,
            &index_issue,
        );

        // recover note hash
        let note_hash = cir.h.var_note(cs.clone(), &note)?;
        // recover blind note hash
        let blind_note_hash = cir.h.var_blind_note(cs.clone(), &note_hash, &blind)?;

        // initial state is asset hash. match it
        pi.state_in
            .conditional_enforce_equal(&pi.asset_hash, &is_issue_tx)?;

        // recover the output state
        let state_out = cir.h.var_state(cs.clone(), &const_zero, &blind_note_hash)?;

        pi.state_out
            .conditional_enforce_equal(&state_out, &is_issue_tx)?;

        // recover sighash
        let sighash = cir
            .h
            .var_sighash(cs.clone(), &const_zero, &const_zero, &note_hash)?;

        (sighash, note_hash, is_issue_tx)
    };

    // Branch 2: SplitTx
    let sighash_split = {
        let is_split_tx = is_issue_tx.not();

        // enforce input state integrity
        let (blind_note_in_hash, note_in_hash, value_in) = {
            let sibling = witness_in(cs.clone(), aux, |e| e.sibling)?;
            let value = witness_in(cs.clone(), aux, |e| E::Field::from(e.value_in))?;
            let blind = witness_in(cs.clone(), aux, |e| e.blind_in)?;
            let parent_note = witness_in(cs.clone(), aux, |e| e.parent)?;

            let index = witness_in(cs.clone(), aux, |e| e.input_index.inner::<E::Field>())?;
            // enforce index to be either ::Out0 or ::Out1
            let is_i0 = index.is_eq(&index_0)?;
            let is_i1 = index.is_eq(&index_1)?;
            is_i0.or(&is_i1)?.enforce_equal(&const_true)?;

            let note_in = NoteVar::new(
                &pi.asset_hash,
                &pi.sender,
                &value,
                &pi.step,
                &parent_note,
                &index,
            );

            // recover note hash
            let note_hash = cir.h.var_note(cs.clone(), &note_in)?;

            // recover blinded note hash
            let blind_note_hash = cir.h.var_blind_note(cs.clone(), &note_hash, &blind)?;

            // recover input state
            let lhs = CondSelectGadget::conditionally_select(&is_i0, &blind_note_hash, &sibling)?;
            let rhs = CondSelectGadget::conditionally_select(&is_i1, &sibling, &blind_note_hash)?;
            let state_in = cir.h.var_state(cs.clone(), &lhs, &rhs)?;

            // match with public input
            pi.state_in
                .conditional_enforce_equal(&state_in, &is_split_tx)?;

            // enforce nullifier integrity
            let nullifier = cir
                .h
                .var_nullifier(cs.clone(), &note_hash, &nullifier_key)?;

            // match with public input
            pi.nullifier
                .conditional_enforce_equal(&nullifier, &is_split_tx)?;

            (blind_note_hash, note_hash, value)
        };

        // enforce output state integrity
        let (note_out_hash_0, note_out_hash_1) = {
            let value_out_1 = witness_in(cs.clone(), aux, |e| E::Field::from(e.value_out))?;
            let blind_1 = witness_in(cs.clone(), aux, |e| e.blind_out_1)?;
            let note_out_1 = NoteVar {
                asset_hash: pi.asset_hash.clone(),
                owner: pi.sender.clone(),
                value: value_out_1.clone(),
                step: pi.step.clone(),
                parent_note: blind_note_in_hash.clone(),
                out_index: index_1,
            };
            // recover note hash
            let note_hash_1 = cir.h.var_note(cs.clone(), &note_out_1)?;

            // recover blinded note hash
            let blind_note_hash_1 = cir.h.var_blind_note(cs.clone(), &note_hash_1, &blind_1)?;
            let value_out_0 = value_in - &value_out_1;

            let max = FpVar::new_constant(cs.clone(), E::Field::from(u64::MAX))?;
            value_out_0.enforce_cmp(&value_out_1, std::cmp::Ordering::Less, true)?;
            value_out_1.enforce_cmp(&max, std::cmp::Ordering::Less, true)?; // maybe not required

            let blind_0 = witness_in(cs.clone(), aux, |e| e.blind_out_0)?;
            let receiver = witness_in(cs.clone(), aux, |e| e.receiver)?;
            let note_out_0 = NoteVar {
                asset_hash: pi.asset_hash.clone(),
                owner: receiver,
                value: value_out_0.clone(),
                step: pi.step.clone(),
                parent_note: blind_note_in_hash,
                out_index: index_0,
            };
            // recover note hash
            let note_hash_0 = cir.h.var_note(cs.clone(), &note_out_0)?;

            // recover blinded note hash
            let blind_note_hash_0 = cir.h.var_blind_note(cs.clone(), &note_hash_1, &blind_0)?;

            // recover the output state
            let state_out = cir
                .h
                .var_state(cs.clone(), &blind_note_hash_0, &blind_note_hash_1)?;

            // match with public input
            pi.state_out
                .conditional_enforce_equal(&state_out, &is_split_tx)?;

            (note_hash_0, note_hash_1)
        };

        // recover sighash
        cir.h.var_sighash(
            cs.clone(),
            &note_in_hash,
            &note_out_hash_0,
            &note_out_hash_1,
        )?
    };

    // select sighash based on the tx type
    let sighash =
        CondSelectGadget::conditionally_select(&is_issue_tx, &sighash_issue, &sighash_split)?;

    // recover signature & verify
    let sig_r = witness_point_in(cs.clone(), aux, |e| *e.signature.r())?;
    let sig_s = NonNativeFieldVar::new_witness(cs.clone(), || {
        aux.map(|e| e.signature.s())
            .ok_or(SynthesisError::AssignmentMissing)
    })?;
    verify_signature(cs.clone(), &cir.h.eddsa, &pubkey, &sig_r, &sig_s, &sighash)?;

    Ok(())
}
