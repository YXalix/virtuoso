//! NUMA topology: `nodes` × `memory_per_node`; `smp` must be divisible by nodes
//! (invalid configs error at parse time, not at runtime).

/// NUMA topology.
#[derive(Debug, Clone)]
pub(crate) struct NumaTopology {
    pub smp: u32,
    pub nodes: u32,
    /// Memory per node, e.g. "1G"; total memory = per-node × nodes
    pub memory_per_node: String,
}

impl NumaTopology {
    pub(crate) fn parse(smp: &str, nodes: &str, memory_per_node: &str) -> anyhow::Result<NumaTopology> {
        let smp: u32 = smp
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("SMP is not a number: {smp}"))?;
        let nodes: u32 = nodes
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("NUMA_NODES is not a number: {nodes}"))?;
        if nodes == 0 {
            anyhow::bail!("NUMA_NODES cannot be 0");
        }
        if nodes > 1 && !smp.is_multiple_of(nodes) {
            anyhow::bail!("SMP ({smp}) not divisible by NUMA_NODES ({nodes})");
        }
        if memory_per_node.is_empty() {
            anyhow::bail!("NUMA_MEMORY is empty");
        }
        Ok(NumaTopology {
            smp,
            nodes,
            memory_per_node: memory_per_node.to_string(),
        })
    }

    /// Total memory string (run-qemu.sh's multiplication rule: numeric prefix × node
    /// count, unit suffix preserved).
    pub(crate) fn total_memory(&self) -> anyhow::Result<String> {
        // common keeps a zero-dep String error surface; lifted to anyhow here
        let (n, unit) =
            crate::util::parse_memory(&self.memory_per_node).map_err(anyhow::Error::msg)?;
        let total = n.saturating_mul(self.nodes as u64);
        Ok(unit.render(total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numa_divisibility_enforced() {
        assert!(NumaTopology::parse("7", "2", "1G").is_err());
        assert!(NumaTopology::parse("8", "2", "1G").is_ok());
        assert!(NumaTopology::parse("x", "2", "1G").is_err());
        assert!(NumaTopology::parse("8", "0", "1G").is_err());
    }

    #[test]
    fn total_memory_suffix_rules() {
        assert_eq!(
            NumaTopology::parse("8", "2", "1G")
                .unwrap()
                .total_memory()
                .unwrap(),
            "2G"
        );
        assert_eq!(
            NumaTopology::parse("8", "2", "512M")
                .unwrap()
                .total_memory()
                .unwrap(),
            "1024M"
        );
        assert_eq!(
            NumaTopology::parse("8", "2", "1024")
                .unwrap()
                .total_memory()
                .unwrap(),
            "2048"
        );
        assert!(NumaTopology::parse("8", "2", "1X")
            .unwrap()
            .total_memory()
            .is_err());
    }
}
