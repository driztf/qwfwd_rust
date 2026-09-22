//! Adaptive Huffman coding used by the Quake III connectionless protocol.
//!
//! This is the algorithm from id's `huffman.c` (Vitter's adaptive scheme with
//! a "not yet transmitted" escape symbol), rebuilt over an index arena. Every
//! quirk of the original update order is preserved on purpose: both ends of a
//! connection must evolve identical trees or the stream becomes garbage.

const NYT: u16 = 256;
const INTERNAL_NODE: u16 = 257;
const MAX_NODES: usize = 768;

type NodeId = usize;
type CellId = usize;

#[derive(Clone, Copy, Default)]
struct Node {
    left: Option<NodeId>,
    right: Option<NodeId>,
    parent: Option<NodeId>,
    next: Option<NodeId>,
    prev: Option<NodeId>,
    /// Shared cell naming the highest ranked node of this node's weight block.
    head: Option<CellId>,
    weight: u32,
    symbol: u16,
}

struct Tree {
    nodes: Vec<Node>,
    cells: Vec<Option<NodeId>>,
    free_cells: Vec<CellId>,
    root: NodeId,
    /// Lowest ranked node of the list, which is always the NYT node.
    lhead: NodeId,
    loc: [Option<NodeId>; 257],
}

impl Tree {
    fn new() -> Self {
        let mut tree = Tree {
            nodes: Vec::with_capacity(MAX_NODES),
            cells: Vec::with_capacity(MAX_NODES),
            free_cells: Vec::new(),
            root: 0,
            lhead: 0,
            loc: [None; 257],
        };
        tree.nodes.push(Node {
            symbol: NYT,
            ..Node::default()
        });
        tree.loc[NYT as usize] = Some(0);
        tree
    }

    fn alloc_node(&mut self) -> NodeId {
        self.nodes.push(Node::default());
        self.nodes.len() - 1
    }

    fn alloc_cell(&mut self) -> CellId {
        self.free_cells.pop().unwrap_or_else(|| {
            self.cells.push(None);
            self.cells.len() - 1
        })
    }

    fn release_cell(&mut self, cell: CellId) {
        self.cells[cell] = None;
        self.free_cells.push(cell);
    }

    fn head_of(&self, node: NodeId) -> CellId {
        self.nodes[node]
            .head
            .expect("ranked node always belongs to a weight block")
    }

    fn replace_child(&mut self, parent: Option<NodeId>, old: NodeId, new: NodeId) {
        match parent {
            Some(p) if self.nodes[p].left == Some(old) => self.nodes[p].left = Some(new),
            Some(p) => self.nodes[p].right = Some(new),
            None => self.root = new,
        }
    }

    // The two child replacements happen in sequence, so swapping siblings can
    // undo itself; that is what the reference implementation does too.
    fn swap_in_tree(&mut self, a: NodeId, b: NodeId) {
        let parent_a = self.nodes[a].parent;
        let parent_b = self.nodes[b].parent;
        self.replace_child(parent_a, a, b);
        self.replace_child(parent_b, b, a);
        self.nodes[a].parent = parent_b;
        self.nodes[b].parent = parent_a;
    }

    fn swap_in_list(&mut self, a: NodeId, b: NodeId) {
        let (a_next, a_prev) = (self.nodes[a].next, self.nodes[a].prev);
        let (b_next, b_prev) = (self.nodes[b].next, self.nodes[b].prev);
        self.nodes[a].next = b_next;
        self.nodes[b].next = a_next;
        self.nodes[a].prev = b_prev;
        self.nodes[b].prev = a_prev;

        if self.nodes[a].next == Some(a) {
            self.nodes[a].next = Some(b);
        }
        if self.nodes[b].next == Some(b) {
            self.nodes[b].next = Some(a);
        }
        if let Some(n) = self.nodes[a].next {
            self.nodes[n].prev = Some(a);
        }
        if let Some(n) = self.nodes[b].next {
            self.nodes[n].prev = Some(b);
        }
        if let Some(n) = self.nodes[a].prev {
            self.nodes[n].next = Some(a);
        }
        if let Some(n) = self.nodes[b].prev {
            self.nodes[n].next = Some(b);
        }
    }

    fn increment(&mut self, node: NodeId) {
        let weight = self.nodes[node].weight;

        if let Some(next) = self.nodes[node].next
            && self.nodes[next].weight == weight
        {
            let leader = self.cells[self.head_of(node)].expect("weight block has a leader");
            if Some(leader) != self.nodes[node].parent {
                self.swap_in_tree(leader, node);
            }
            self.swap_in_list(leader, node);
        }

        let head = self.head_of(node);
        match self.nodes[node].prev {
            Some(prev) if self.nodes[prev].weight == weight => self.cells[head] = Some(prev),
            _ => self.release_cell(head),
        }

        let weight = weight + 1;
        self.nodes[node].weight = weight;
        match self.nodes[node].next {
            Some(next) if self.nodes[next].weight == weight => {
                self.nodes[node].head = self.nodes[next].head;
            }
            _ => {
                let cell = self.alloc_cell();
                self.cells[cell] = Some(node);
                self.nodes[node].head = Some(cell);
            }
        }

        if let Some(parent) = self.nodes[node].parent {
            self.increment(parent);
            if self.nodes[node].prev == Some(parent) {
                self.swap_in_list(node, parent);
                let head = self.head_of(node);
                if self.cells[head] == Some(node) {
                    self.cells[head] = Some(parent);
                }
            }
        }
    }

    /// Inserts `node` right above the NYT node in the rank list, joining the
    /// weight-1 block if one is already there.
    fn link_above_nyt(&mut self, node: NodeId, fallback_leader: NodeId) {
        let lhead = self.lhead;
        let above = self.nodes[lhead].next;
        self.nodes[node].next = above;
        match above {
            Some(a) => {
                self.nodes[a].prev = Some(node);
                if self.nodes[a].weight == 1 {
                    self.nodes[node].head = self.nodes[a].head;
                } else {
                    let cell = self.alloc_cell();
                    self.cells[cell] = Some(fallback_leader);
                    self.nodes[node].head = Some(cell);
                }
            }
            None => {
                let cell = self.alloc_cell();
                self.cells[cell] = Some(node);
                self.nodes[node].head = Some(cell);
            }
        }
        self.nodes[lhead].next = Some(node);
        self.nodes[node].prev = Some(lhead);
    }

    /// Counts one more occurrence of `ch`, growing the tree on first sight.
    fn add_ref(&mut self, ch: u8) {
        if let Some(node) = self.loc[ch as usize] {
            self.increment(node);
            return;
        }

        let lhead = self.lhead;
        let leaf = self.alloc_node();
        let branch = self.alloc_node();

        self.nodes[branch].symbol = INTERNAL_NODE;
        self.nodes[branch].weight = 1;
        self.link_above_nyt(branch, branch);

        self.nodes[leaf].symbol = u16::from(ch);
        self.nodes[leaf].weight = 1;
        self.link_above_nyt(leaf, branch);

        let parent = self.nodes[lhead].parent;
        self.replace_child(parent, lhead, branch);
        self.nodes[branch].right = Some(leaf);
        self.nodes[branch].left = Some(lhead);
        self.nodes[branch].parent = parent;
        self.nodes[lhead].parent = Some(branch);
        self.nodes[leaf].parent = Some(branch);
        self.loc[ch as usize] = Some(leaf);

        if let Some(p) = parent {
            self.increment(p);
        }
    }

    fn emit_path(&self, node: NodeId, child: Option<NodeId>, out: &mut BitWriter) {
        if let Some(parent) = self.nodes[node].parent {
            self.emit_path(parent, Some(node), out);
        }
        if let Some(child) = child {
            out.put(u8::from(self.nodes[node].right == Some(child)));
        }
    }

    fn transmit(&self, symbol: u16, out: &mut BitWriter) {
        match self.loc[symbol as usize] {
            Some(node) => self.emit_path(node, None, out),
            None => {
                self.transmit(NYT, out);
                for i in (0..8).rev() {
                    out.put(((symbol >> i) & 1) as u8);
                }
            }
        }
    }

    fn receive(&self, input: &mut BitReader) -> u16 {
        let mut node = Some(self.root);
        while let Some(n) = node
            && self.nodes[n].symbol == INTERNAL_NODE
        {
            node = if input.get() == 1 {
                self.nodes[n].right
            } else {
                self.nodes[n].left
            };
        }
        node.map_or(0, |n| self.nodes[n].symbol)
    }
}

struct BitWriter {
    buf: Vec<u8>,
    pos: usize,
}

impl BitWriter {
    fn put(&mut self, bit: u8) {
        if self.pos & 7 == 0 {
            self.buf.push(0);
        }
        let index = self.pos >> 3;
        self.buf[index] |= bit << (self.pos & 7);
        self.pos += 1;
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn get(&mut self) -> u8 {
        let bit = self
            .buf
            .get(self.pos >> 3)
            .map_or(0, |&b| (b >> (self.pos & 7)) & 1);
        self.pos += 1;
        bit
    }
}

/// Compresses `msg[offset..]` in place, prefixing the two byte big-endian
/// uncompressed length Q3 expects.
pub fn compress(msg: &mut Vec<u8>, offset: usize) {
    let Some(input) = msg.get(offset..) else {
        return;
    };
    if input.is_empty() {
        return;
    }

    let mut out = BitWriter {
        buf: vec![(input.len() >> 8) as u8, input.len() as u8],
        pos: 16,
    };
    let mut tree = Tree::new();
    for &b in input {
        tree.transmit(u16::from(b), &mut out);
        tree.add_ref(b);
    }

    let out_len = (out.pos >> 3) + 1;
    out.buf.resize(out_len, 0);
    msg.truncate(offset);
    msg.extend_from_slice(&out.buf);
}

/// Decompresses `msg[offset..]` in place, never growing the message past `max_size`.
pub fn decompress(msg: &mut Vec<u8>, offset: usize, max_size: usize) {
    let Some(input) = msg.get(offset..) else {
        return;
    };
    if input.is_empty() {
        return;
    }

    let declared = (usize::from(input[0]) << 8) + usize::from(input.get(1).copied().unwrap_or(0));
    let out_len = declared.min(max_size.saturating_sub(offset));
    let mut reader = BitReader {
        buf: input,
        pos: 16,
    };
    let mut tree = Tree::new();
    let mut out = Vec::with_capacity(out_len);

    for _ in 0..out_len {
        if (reader.pos >> 3) > input.len() {
            out.push(0);
            break;
        }
        let mut symbol = tree.receive(&mut reader);
        if symbol == NYT {
            symbol = 0;
            for _ in 0..8 {
                symbol = (symbol << 1) | u16::from(reader.get());
            }
        }
        out.push(symbol as u8);
        tree.add_ref(symbol as u8);
    }
    out.resize(out_len, 0);

    msg.truncate(offset);
    msg.extend_from_slice(&out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(payload: &[u8]) {
        let header = b"\xff\xff\xff\xffconnect ";
        let mut msg = header.to_vec();
        msg.extend_from_slice(payload);
        compress(&mut msg, header.len());
        assert_eq!(&msg[..header.len()], header);
        decompress(&mut msg, header.len(), 8192);
        assert_eq!(&msg[header.len()..], payload);
    }

    #[test]
    fn roundtrips_various_payloads() {
        roundtrip(b"a");
        roundtrip(b"\"\\name\\player\\challenge\\12345\\prx\\host\"\0");
        roundtrip(&(0..=255u8).collect::<Vec<_>>());
        roundtrip(&vec![0x41; 3000]);
        let pseudo: Vec<u8> = (0..4000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        roundtrip(&pseudo);
    }

    #[test]
    fn matches_reference_c_implementation() {
        // Expected bytes were produced by the original huff.c (Huff_EncryptPacket at offset 12).
        fn check(payload: &[u8], expected_hex: &[&str]) {
            let expected: Vec<u8> = expected_hex
                .concat()
                .as_bytes()
                .chunks(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect();
            let mut msg = b"\xff\xff\xff\xffconnect ".to_vec();
            msg.extend_from_slice(payload);
            compress(&mut msg, 12);
            assert_eq!(
                msg[12..],
                expected[..],
                "compressed bytes differ from huff.c"
            );
            decompress(&mut msg, 12, 8192);
            assert_eq!(&msg[12..], payload);
        }

        check(b"a", &["00018600"]);
        check(
            b"\"\\name\\player\\challenge\\12345\\prx\\host\"\0",
            &[
                "00284474b08b216cc79470001b1c4f16278cb1b0582340cce38351610a98191683b52e281e0f843de0dc71210f00",
            ],
        );
        check(
            &(0..=255u8).collect::<Vec<_>>(),
            &[
                "01000000010a30400634c0041c200ee4802ad001c6c00a38020f101ec40369411e501ad4022d410f301c4c034bc11670",
                "185c020fc107083e840fa20bf1816443ba209b900f283a940daa0a758126439ba08bd007183c8c0e260bb38145c3aa60",
                "93b00b38389c0c2e0a378107c38be083f003047e043f845e841f446e442fc44ec40f24762437a456a41764666427e446",
                "e407147a143b945a941b546a542bd44ad40b34723433b452b41374627423f442f4030c7c0c3d8c5c8c1d4c6c4c2dcc4c",
                "cc0d2c742c35ac54ac156c646c25ec44ec051c781c399c589c195c685c29dc48dc093c703c31bc50bc117c607c21fc40",
                "fc0102fe04fe08fa12fc21e44de88bb027e10f22ee44de88ba127d21e64cec89b823f10712f624ee48da92bc21654dea",
                "8ab425e90b32e664cec89a923d21674cee88bc21f9030afa14f628ea52dca1a44d698bb226e50d2aea54d6a8aa525da1",
                "a64c6d89ba22f5051af234e668cad29ca1254d6b8ab624ed093ae274c6e88ad21da1274c6f88be20fd0106fc0cfa18f2",
                "32ec61c4cda88b3127e30e26ec4cda98b2326d61c6ccac893923f30616f42cea58d2b2ac6145cdaa8a3525eb0a36e46c",
                "cad892b22d6147ccae883d21fb020ef81cf238e272cce184cd298b3326e70c2ee85cd2b8a2724de186cc2d893b22f704",
                "1ef03ce278c2f28ce105cd2b8a3724ef083ee07cc2f882f20de107cc2f883f20ff00",
            ],
        );
        check(
            &[0x41; 300],
            &["012c82ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff07"],
        );
        let pseudo: Vec<u8> = (0..1000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        check(
            &pseudo,
            &[
                "03e800ba710f33eea73acc8408f64ed6502b8861d6440a5102087a1e8f836579186a1af38238d90fb21d7503499913a2",
                "18310208e9053c3fac8fc74bedc16243b23898e40c343ac88df94a74413c039ce88b7805d83c4f8e3a8ba5815473a8c8",
                "95680a50380b8c188a040104d38ff086b0039e7fea3fac5ec81e8e6ef22eb44ed00e167762372457401706677c263846",
                "5e061a7a6c3ac85b8e1bf26bb42bd04b960ae272a432c05286127c6338235e431a036c7d283c4e5c0a1c746c302c964d",
                "e20da475c0358655fc14b864de249a44ec0428794e390a597419306956281248640820704630fc51b811de619a21ec41",
                "a800cefe14fdd0fb82f561f54de20bd927900f46ef04dce0b9f271a1e58cc3099a23e7078af7e8efc0deb2be21698ddc",
                "0aa825630b82e6f0cef89cd23ac1710ced888b2125030cfae0f4d8e892dc41be0d748bf126c10df8ea7cd568abe25481",
                "aecc6589d222860570f36ce648c9a2900126cd448a9024fc09bee3b4c7708e421fe12a4c5908a3207801b6fca4fa50f4",
                "02eb61d24da80b01273f0e3aecb8d860b0726da1dc8cb1097c23db06d2f428e980d332a5214c8d908a5f259d0a5ce530",
                "cab8955221c1440c81881d21190294f9c0f398e612ce4198cd1f8b2e266e0c98e8dcd1a8a4624a8190cc2e894c222a04",
                "10f0cce088c02282e1dfaf8f6f0f4d951ededd1c0cd5383b393ab9b7b3b5b1bcb2b430b73a35313632b4b9b3babab6ad",
                "a5a9a1aea6aaa2aca4a8202f272b232d2529212e262a222c3ebc38b9b13031beb62120a841464244230c88011111f563",
                "ddd250535154909391121713111210e7e1e260536262a0a391a020232122c0c3c1c24043414280832186a14706634767",
                "a02441e1a6c686c624e1e4d0af0b6a0e032f2b5fd6a4b8a8b03c0894e3e233f225340288484b4e4380008f8d0c6b0493",
                "0e868c080f0b0d098e980c0c1d1d050e9a045f97114423a83c70910901f1d010b061316190b190787046900e67441736",
                "6ea8c880ba30928e8581e30166049b115d887060d874816103038309048b4222e0c05008183412f0fff7fbf3fdf5f9f1",
                "fef6faf2fcf4f8707f777b737d7579717e767ac2e8e8f060bfdfeecef6d6818df5b5d5d5e5a5c585a5b9d999e9d9c989",
                "f1b189fec34383035dfb7a7bba7d7776b4b7b5b634373536d4d7d5d6545755569497959614171516e4e7e5e6cdcecacc",
                "c89c969a92ec2c31213e2e2e263a2a323a3c2c34242c2830c03fd0a38fb797a77377375717b64e8e0ef676b636d65696",
                "16e666a626c6468606fa7aba3ada5e3435d4d5d4aa282b29ea92979395519292941097111511161217e0e7e315e2e6e2",
                "e4e065cccac2cc4493819e8e96466a2a4a0a72325212622242027c3c5c1c6c2c4c0c74fe8371366839c15e1851f90fc6",
                "f1cd09f6e2ef1bc7cf09d6e5ef1bc7670bd6e5ef3bc0670bd6e547e90bc66743d6e5ef0b46c267d3e5ef0bc667d3e5ef",
                "0bc667d3e5ef0bc66743e4ef0b464260e3674385e767e387e7e7e7e7c7c7c7c7c70f0e0e0e0e6663636363e3e7e7e7e7",
                "e787e3e7e7e787cdcecacc80494f4b4d494e4a4c888f8b8d898e8a8c080f0b0d810e0a0c8085f2f3f58183f4f4708772",
                "7571767271b0b7b3b5b5b6b2b4b03233353136863034d0d703d7d1d6d204d35057535551565254909793959196929410",
                "1713151116121400e5e3e50105e1e2e400f9cbcac2fc9791819e8e81869a8a92869c8c94849c88f0ef8f0888e0f7e7fb",
                "3dded7e7c7b76faf2fcf4f8f0ff77700",
            ],
        );
        let mix: Vec<u8> = (0..64)
            .map(|i| b"abcabcabdabeabfaaaaaaabbbbbbccccccccc"[i % 37])
            .collect();
        check(
            &mix,
            &["0040868c30761d1387a9831950fd93b6ed3dcfc3f1d11faa06"],
        );
    }

    #[test]
    fn empty_payload_is_untouched() {
        let mut msg = b"\xff\xff\xff\xff".to_vec();
        compress(&mut msg, 4);
        assert_eq!(msg, b"\xff\xff\xff\xff");
        decompress(&mut msg, 4, 8192);
        assert_eq!(msg, b"\xff\xff\xff\xff");
        compress(&mut msg, 10);
        assert_eq!(msg, b"\xff\xff\xff\xff");
    }

    #[test]
    fn truncated_input_does_not_panic() {
        let mut msg = vec![0x00, 0x40];
        decompress(&mut msg, 0, 8192);
        assert_eq!(msg.len(), 0x40);
        let mut msg = vec![0xff, 0xff, 0x12];
        decompress(&mut msg, 0, 64);
        assert_eq!(msg.len(), 64);
    }
}
