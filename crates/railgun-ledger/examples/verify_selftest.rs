//! Self-test of the smoke test's verification equation against the software signer.
use ark_ff::{BigInteger, PrimeField};
use num_bigint::{BigInt, Sign};
use ruint::aliases::U256;

fn big(u: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &u.to_be_bytes::<32>())
}

fn f2u(f: ark_bn254::Fr) -> U256 {
    let mut b = f.into_bigint().to_bytes_le();
    b.resize(32, 0);
    b.reverse();
    U256::from_be_slice(&b)
}

fn main() {
    let sk = crypto::babyjubjub::PrivateKey::new([1u8; 32]);
    let pk = sk.public();
    let message = U256::from(42u64);
    let sig = sk.sign(big(message)).unwrap();

    let hm = crypto::poseidon_hash(&[f2u(sig.r_b8.x), f2u(sig.r_b8.y), f2u(pk.x), f2u(pk.y), message])
        .unwrap();

    let left = crypto::babyjubjub::b8().mul_scalar(&sig.s);
    let a = crypto::babyjubjub::Point { x: pk.x, y: pk.y };
    let right = sig
        .r_b8
        .projective()
        .add(&a.mul_scalar(&(big(hm) * 8)).projective())
        .affine();

    println!(
        "verify equation on software signature: {}",
        if left.x == right.x && left.y == right.y { "PASS" } else { "FAIL" }
    );
}
