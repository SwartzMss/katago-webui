use super::{BoardStones, GameMeta, InitialSetup, MoveNode, ParsedReview, StoneColor};
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet, VecDeque};

pub fn parse_sgf(input: &str) -> Result<ParsedReview> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty SGF input");
    }

    let mut parser = SgfParser::new(trimmed);
    let nodes = parser.parse_main_sequence()?;
    if nodes.is_empty() {
        bail!("SGF contains no nodes");
    }
    let root = &nodes[0];

    let mut board_size: u32 = 19;
    if let Some(sz_val) = root.single_value("SZ") {
        if let Ok(sz) = sz_val.parse::<u32>() {
            if (5..=25).contains(&sz) {
                board_size = sz;
            }
        }
    }

    let mut komi = 0.0_f32;
    if let Some(km_val) = root.single_value("KM") {
        if let Ok(km) = km_val.parse::<f32>() {
            komi = km;
        }
    }

    let mut meta = GameMeta::default();
    meta.black = root
        .single_value("PB")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    meta.white = root
        .single_value("PW")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    meta.result = root
        .single_value("RE")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    meta.rules = root
        .single_value("RU")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if komi != 0.0 {
        meta.komi = Some(komi);
    }
    let comment = root
        .single_value("C")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    meta.comment = comment;

    let size_usize = board_size as usize;
    let mut initial_setup = InitialSetup::default();
    initial_setup.black = root
        .values("AB")
        .into_iter()
        .filter_map(|raw| normalise_coord(&raw))
        .collect();
    initial_setup.white = root
        .values("AW")
        .into_iter()
        .filter_map(|raw| normalise_coord(&raw))
        .collect();
    initial_setup.empty = root
        .values("AE")
        .into_iter()
        .filter_map(|raw| normalise_coord(&raw))
        .collect();
    if let Some(pl) = root.single_value("PL") {
        let s = pl.trim();
        if s.eq_ignore_ascii_case("B") {
            initial_setup.to_play = Some(StoneColor::Black);
        } else if s.eq_ignore_ascii_case("W") {
            initial_setup.to_play = Some(StoneColor::White);
        }
    }

    let mut moves: Vec<MoveNode> = Vec::new();
    let mut move_index: u32 = 0;
    for node in nodes.iter().skip(1) {
        if let Some(mn) = node.single_value("MN") {
            if let Ok(idx) = mn.parse::<u32>() {
                move_index = idx.saturating_sub(1);
            }
        }
        if let Some(value) = node.single_value("B") {
            move_index += 1;
            let coord = normalise_coord(&value);
            let comment = node
                .single_value("C")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            moves.push(MoveNode {
                index: move_index,
                color: StoneColor::Black,
                coord,
                comment,
            });
        } else if let Some(value) = node.single_value("W") {
            move_index += 1;
            let coord = normalise_coord(&value);
            let comment = node
                .single_value("C")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            moves.push(MoveNode {
                index: move_index,
                color: StoneColor::White,
                coord,
                comment,
            });
        }
    }

    let final_stones = board_stones_after(size_usize, &initial_setup, &moves, moves.len())?;

    Ok(ParsedReview {
        board_size,
        komi,
        meta,
        moves,
        initial_setup,
        final_stones,
    })
}

pub fn board_stones_after(
    size: usize,
    setup: &InitialSetup,
    moves: &[MoveNode],
    upto: usize,
) -> Result<BoardStones> {
    let board = board_vector_after(size, setup, moves, upto)?;
    let mut black = Vec::new();
    let mut white = Vec::new();
    for y in 0..size {
        for x in 0..size {
            match board[index(size, x, y)] {
                Some(StoneColor::Black) => black.push(point_to_coord(x, y)),
                Some(StoneColor::White) => white.push(point_to_coord(x, y)),
                None => {}
            }
        }
    }
    Ok(BoardStones { black, white })
}

fn board_vector_after(
    size: usize,
    setup: &InitialSetup,
    moves: &[MoveNode],
    upto: usize,
) -> Result<Vec<Option<StoneColor>>> {
    if size == 0 {
        bail!("invalid board size");
    }
    let mut board: Vec<Option<StoneColor>> = vec![None; size * size];
    for coord in &setup.black {
        if let Some((x, y)) = coord_to_point(coord, size) {
            board[index(size, x, y)] = Some(StoneColor::Black);
        }
    }
    for coord in &setup.white {
        if let Some((x, y)) = coord_to_point(coord, size) {
            board[index(size, x, y)] = Some(StoneColor::White);
        }
    }
    for coord in &setup.empty {
        if let Some((x, y)) = coord_to_point(coord, size) {
            board[index(size, x, y)] = None;
        }
    }

    for mv in moves.iter().take(upto.min(moves.len())) {
        let color = mv.color;
        let Some(coord_str) = mv.coord.as_ref() else {
            continue;
        };
        let Some((x, y)) = coord_to_point(coord_str, size) else {
            continue;
        };
        apply_move(&mut board, size, x, y, color);
    }
    Ok(board)
}

fn apply_move(
    board: &mut [Option<StoneColor>],
    size: usize,
    x: usize,
    y: usize,
    color: StoneColor,
) {
    let idx = index(size, x, y);
    if board[idx].is_some() {
        // overwrite illegal placement by replacing the stone to keep board consistent
    }
    board[idx] = Some(color);
    let opponent = color.opponent();
    let neighbors = neighbors(x, y, size);
    let mut captured_any = false;
    for (nx, ny) in neighbors {
        let n_idx = index(size, nx, ny);
        if board[n_idx] == Some(opponent) {
            let (group, liberties) = collect_group(board, size, nx, ny, opponent);
            if liberties == 0 {
                captured_any = true;
                for (gx, gy) in group {
                    board[index(size, gx, gy)] = None;
                }
            }
        }
    }

    let (own_group, liberties) = collect_group(board, size, x, y, color);
    if liberties == 0 && !captured_any {
        for (gx, gy) in own_group {
            board[index(size, gx, gy)] = None;
        }
    }
}

fn collect_group(
    board: &[Option<StoneColor>],
    size: usize,
    x: usize,
    y: usize,
    color: StoneColor,
) -> (Vec<(usize, usize)>, usize) {
    let mut visited: HashSet<(usize, usize)> = HashSet::new();
    let mut liberties: HashSet<(usize, usize)> = HashSet::new();
    let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
    queue.push_back((x, y));

    while let Some((cx, cy)) = queue.pop_front() {
        if !visited.insert((cx, cy)) {
            continue;
        }
        for (nx, ny) in neighbors(cx, cy, size) {
            let idx = index(size, nx, ny);
            match board[idx] {
                Some(stone) if stone == color => {
                    queue.push_back((nx, ny));
                }
                Some(_) => {}
                None => {
                    liberties.insert((nx, ny));
                }
            }
        }
    }

    (visited.into_iter().collect(), liberties.len())
}

fn neighbors(x: usize, y: usize, size: usize) -> Vec<(usize, usize)> {
    let mut result = Vec::with_capacity(4);
    if x > 0 {
        result.push((x - 1, y));
    }
    if y > 0 {
        result.push((x, y - 1));
    }
    if x + 1 < size {
        result.push((x + 1, y));
    }
    if y + 1 < size {
        result.push((x, y + 1));
    }
    result
}

fn index(size: usize, x: usize, y: usize) -> usize {
    y * size + x
}

fn coord_to_point(coord: &str, size: usize) -> Option<(usize, usize)> {
    if coord.len() != 2 {
        return None;
    }
    let mut chars = coord.chars();
    let cx = chars.next()?;
    let cy = chars.next()?;
    let x = (cx as u32).wrapping_sub('a' as u32) as isize;
    let y = (cy as u32).wrapping_sub('a' as u32) as isize;
    if x < 0 || y < 0 {
        return None;
    }
    let (x, y) = (x as usize, y as usize);
    if x >= size || y >= size {
        return None;
    }
    Some((x, y))
}

fn point_to_coord(x: usize, y: usize) -> String {
    let cx = (b'a' + x as u8) as char;
    let cy = (b'a' + y as u8) as char;
    format!("{}{}", cx, cy)
}

fn normalise_coord(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.len() != 2 {
        return None;
    }
    Some(lower)
}

struct Node {
    props: HashMap<String, Vec<String>>,
}

impl Node {
    fn single_value(&self, key: &str) -> Option<String> {
        self.props
            .get(key)
            .and_then(|values| values.first().cloned())
    }

    fn values(&self, key: &str) -> Vec<String> {
        self.props.get(key).cloned().unwrap_or_default()
    }
}

struct SgfParser<'a> {
    bytes: &'a [u8],
    idx: usize,
    len: usize,
}

impl<'a> SgfParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            idx: 0,
            len: input.len(),
        }
    }

    fn parse_main_sequence(&mut self) -> Result<Vec<Node>> {
        self.skip_whitespace();
        if !self.consume_char('(') {
            bail!("SGF must start with '('");
        }
        let mut nodes = Vec::new();
        loop {
            self.skip_whitespace();
            match self.peek_char() {
                Some(';') => {
                    nodes.push(self.parse_node()?);
                }
                Some('(') => {
                    self.skip_subtree()?;
                }
                Some(')') => {
                    self.idx += 1;
                    break;
                }
                Some(_) => {
                    self.idx += 1;
                }
                None => break,
            }
        }
        Ok(nodes)
    }

    fn parse_node(&mut self) -> Result<Node> {
        if !self.consume_char(';') {
            bail!("expected ';' to start node");
        }
        let mut props: HashMap<String, Vec<String>> = HashMap::new();
        loop {
            self.skip_whitespace();
            let name = match self.parse_ident()? {
                Some(n) => n,
                None => break,
            };
            let values = self.parse_values()?;
            let entry = props.entry(name).or_insert_with(Vec::new);
            entry.extend(values);
        }
        Ok(Node { props })
    }

    fn parse_ident(&mut self) -> Result<Option<String>> {
        self.skip_whitespace();
        let mut ident = String::new();
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_uppercase() {
                ident.push(ch);
                self.idx += 1;
            } else {
                break;
            }
        }
        if ident.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ident))
        }
    }

    fn parse_values(&mut self) -> Result<Vec<String>> {
        let mut values = Vec::new();
        loop {
            self.skip_whitespace();
            if !self.consume_char('[') {
                break;
            }
            let mut value = String::new();
            let mut escaped = false;
            while let Some(byte) = self.next_byte() {
                if escaped {
                    escaped = false;
                    match byte {
                        b'n' | b'r' => value.push('\n'),
                        b't' => value.push('\t'),
                        b']' => value.push(']'),
                        other => value.push(other as char),
                    }
                    continue;
                }
                match byte {
                    b'\\' => {
                        escaped = true;
                    }
                    b']' => break,
                    b'\r' => {
                        if self.peek_byte() == Some(b'\n') {
                            self.idx += 1;
                        }
                        value.push('\n');
                    }
                    b'\n' => value.push('\n'),
                    other => value.push(other as char),
                }
            }
            values.push(value);
        }
        Ok(values)
    }

    fn skip_subtree(&mut self) -> Result<()> {
        if !self.consume_char('(') {
            bail!("expected '(' for subtree");
        }
        let mut depth = 1i32;
        let mut in_value = false;
        let mut escaped = false;
        while let Some(byte) = self.next_byte() {
            if in_value {
                if escaped {
                    escaped = false;
                    continue;
                }
                match byte {
                    b'\\' => escaped = true,
                    b']' => in_value = false,
                    _ => {}
                }
                continue;
            }
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                b'[' => in_value = true,
                _ => {}
            }
        }
        if depth != 0 {
            bail!("unterminated subtree");
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_whitespace() {
                self.idx += 1;
            } else {
                break;
            }
        }
    }

    fn consume_char(&mut self, target: char) -> bool {
        if self.peek_char() == Some(target) {
            self.idx += 1;
            true
        } else {
            false
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.bytes.get(self.idx).map(|b| *b as char)
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.idx).copied()
    }

    fn next_byte(&mut self) -> Option<u8> {
        if self.idx >= self.len {
            None
        } else {
            let b = self.bytes[self.idx];
            self.idx += 1;
            Some(b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_game() {
        let sgf = "(;GM[1]SZ[9]KM[6.5]PB[Black]PW[White];B[dd];W[ee];B[cc])";
        let parsed = parse_sgf(sgf).expect("parse sgf");
        assert_eq!(parsed.board_size, 9);
        assert_eq!(parsed.komi, 6.5);
        assert_eq!(parsed.meta.black.as_deref(), Some("Black"));
        assert_eq!(parsed.meta.white.as_deref(), Some("White"));
        assert_eq!(parsed.moves.len(), 3);
        assert_eq!(parsed.moves[0].coord.as_deref(), Some("dd"));
        assert_eq!(parsed.moves[1].coord.as_deref(), Some("ee"));
        assert_eq!(parsed.moves[2].coord.as_deref(), Some("cc"));
        assert!(parsed.final_stones.black.contains(&"dd".to_string()));
        assert!(parsed.final_stones.black.contains(&"cc".to_string()));
        assert!(parsed.final_stones.white.contains(&"ee".to_string()));
    }

    #[test]
    fn parse_with_setup_and_pass() {
        let sgf = "(;SZ[5]AB[aa][bb]AW[cc];B[dd];W[];B[ee])";
        let parsed = parse_sgf(sgf).expect("parse sgf");
        assert_eq!(parsed.initial_setup.black.len(), 2);
        assert_eq!(parsed.initial_setup.white.len(), 1);
        assert_eq!(parsed.moves.len(), 3);
        assert_eq!(parsed.moves[1].coord, None); // pass move
        assert!(parsed.final_stones.black.contains(&"aa".to_string()));
        assert!(parsed.final_stones.black.contains(&"bb".to_string()));
        assert!(parsed.final_stones.black.contains(&"dd".to_string()));
        assert!(parsed.final_stones.black.contains(&"ee".to_string()));
        assert!(parsed.final_stones.white.contains(&"cc".to_string()));
    }
}
