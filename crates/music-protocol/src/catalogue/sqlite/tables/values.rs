//! Typed cell access for rows of the materialized SQL tables.
use super::{Cell, Row, Table};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
pub fn set(t: &Table, r: &mut Row, k: &str, v: Cell) -> Result<()> {
    r[t.column(k)?] = v;
    Ok(())
}
pub fn int(t: &Table, r: &Row, k: &str) -> Result<i64> {
    t.at(r, k)?.integer()
}
pub fn real(c: &Cell) -> Result<f64> {
    let Cell::Real(bits) = c else {
        anyhow::bail!("expected real storage")
    };
    let v = f64::from_bits(*bits);
    ensure!(v.is_finite(), "nonfinite real");
    Ok(v)
}
pub fn unique<'a>(t: &'a Table, k: &str, v: Cell) -> Result<&'a Row> {
    let c = t.column(k)?;
    let mut it = t.rows.iter().filter(|(r, _)| r[c] == v);
    let (r, n) = it.next().with_context(|| format!("missing {k}"))?;
    ensure!(*n == 1 && it.next().is_none(), "nonunique {k}");
    Ok(r)
}
pub fn absent(t: &Table, k: &str, v: Cell) -> Result<bool> {
    let c = t.column(k)?;
    Ok(t.rows.keys().all(|r| r[c] != v))
}
pub fn decimal(t: &Table, k: &str) -> Result<i64> {
    let s = t
        .at(unique(t, "key", Cell::Text(k.into()))?, "value")?
        .text()?;
    let n = s.parse::<i64>()?;
    ensure!(n.to_string() == s, "noncanonical property");
    Ok(n)
}
pub fn replace(t: &mut BTreeMap<String, Table>, name: &str, old: &Row, new: Row) -> Result<()> {
    let t = t.get_mut(name).context("expected table absent")?;
    ensure!(
        t.rows.remove(old) == Some(1) && t.rows.insert(new, 1).is_none(),
        "replacement multiplicity differs"
    );
    Ok(())
}
