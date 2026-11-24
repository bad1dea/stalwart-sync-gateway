use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    Left,
    Right,
    And,
    Or,
    Not,
    Words,
    All,
    Any,
    None,
    Comma,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PhraseMatch {
    All,
    Any,
    None,
    Expression,
}

pub(super) fn to_filter(input: &str) -> Result<Value, &'static str> {
    let tokens = tokenize(input)?;
    let phrases = tokens
        .iter()
        .filter_map(|token| match token {
            Token::Word(word) => {
                let word = word
                    .strip_prefix('+')
                    .or_else(|| word.strip_prefix('-'))
                    .unwrap_or(word);
                search_phrase_word(word)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !phrases.is_empty() && phrases_for_postfilter(input).is_none() {
        return Err(
            "quoted phrases require an AND-only expression with unambiguous property grouping",
        );
    }
    let mut parser = Parser {
        tokens,
        position: 0,
    };
    let filter = parser.implicit(0)?;
    if parser.position != parser.tokens.len() {
        return Err("unexpected search token");
    }
    Ok(filter)
}

pub(super) fn phrases_for_postfilter(input: &str) -> Option<(PhraseMatch, Vec<(String, String)>)> {
    let tokens = tokenize(input).ok()?;
    if let [Token::Not, Token::Word(word)] = tokens.as_slice() {
        return Some((PhraseMatch::None, vec![search_phrase_word(word)?]));
    }
    if let [Token::Word(word)] = tokens.as_slice() {
        if let Some(negative) = word.strip_prefix('-') {
            return Some((PhraseMatch::None, vec![search_phrase_word(negative)?]));
        }
    }
    if tokens.len() > 1 && tokens.iter().all(|token| matches!(token, Token::Word(_))) {
        let terms = tokens
            .iter()
            .filter_map(|token| match token {
                Token::Word(word) => search_phrase_word(word.strip_prefix('+').unwrap_or(word)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if terms.len() == tokens.len() && terms.iter().all(|(property, _)| property == &terms[0].0)
        {
            return Some((PhraseMatch::Any, terms));
        }
    }
    if tokens.len() > 2 && tokens.iter().all(|token| matches!(token, Token::Word(_))) {
        if let Some(terms) = implicit_phrase_boolean_groups(&tokens) {
            return Some((PhraseMatch::Expression, terms));
        }
    }
    if tokens.contains(&Token::Or)
        && !tokens.contains(&Token::And)
        && is_flat_or_expression(&tokens)
        && tokens
            .iter()
            .all(|token| matches!(token, Token::Word(_) | Token::Or | Token::Not))
    {
        let mut terms = Vec::new();
        let mut phrase_count = 0;
        let mut position = 0;
        let mut term_count = 0;
        let mut has_negated_phrase = false;
        while position < tokens.len() {
            let negated = tokens.get(position) == Some(&Token::Not);
            if negated {
                position += 1;
            }
            let Token::Word(word) = tokens.get(position)? else {
                return None;
            };
            if negated {
                if word.starts_with(['+', '-']) {
                    return None;
                }
                if let Some((property, value)) = search_phrase_word(word) {
                    terms.push((format!("$not${property}"), value));
                    phrase_count += 1;
                    has_negated_phrase = true;
                } else {
                    let (property, value) =
                        boolean_search_term(word).or_else(|| importance_search_term(word))?;
                    terms.push((format!("$not{property}"), value));
                }
            } else {
                let positive = word.strip_prefix('+').unwrap_or(word);
                if let Some(phrase) = search_phrase_word(positive) {
                    terms.push(phrase);
                    phrase_count += 1;
                } else if let Some(negative) = word.strip_prefix('-') {
                    if let Some((property, value)) = search_phrase_word(negative) {
                        terms.push((format!("$not${property}"), value));
                        phrase_count += 1;
                        has_negated_phrase = true;
                    } else {
                        let (property, value) = boolean_search_term(negative)
                            .or_else(|| importance_search_term(negative))?;
                        terms.push((format!("$not{property}"), value));
                    }
                } else {
                    let term =
                        boolean_search_term(word).or_else(|| importance_search_term(word))?;
                    terms.push(term);
                }
            }
            term_count += 1;
            position += 1;
            if position < tokens.len() {
                if tokens.get(position) != Some(&Token::Or) {
                    return None;
                }
                position += 1;
            }
        }
        if term_count > 1 && phrase_count > 0 {
            if has_negated_phrase {
                let mut expression = Vec::with_capacity(terms.len() * 2 - 1);
                for (index, term) in terms.into_iter().enumerate() {
                    expression.push(term);
                    if index > 0 {
                        expression.push(("$op$or".to_owned(), String::new()));
                    }
                }
                return Some((PhraseMatch::Expression, expression));
            }
            return Some((PhraseMatch::Any, terms));
        }
        return None;
    }
    if tokens.contains(&Token::Or)
        && tokens.iter().any(|token| {
            matches!(token, Token::Word(word) if search_phrase_word(word.strip_prefix('+').or_else(|| word.strip_prefix('-')).unwrap_or(word)).is_some())
        })
    {
        return flat_phrase_boolean_expression(&tokens)
            .map(|terms| (PhraseMatch::Expression, terms));
    }
    if tokens
        .iter()
        .enumerate()
        .any(|(index, token)| match token {
            Token::Word(_)
            | Token::And
            | Token::Words
            | Token::Left
            | Token::Right
            | Token::Comma => false,
            Token::Not => !matches!(tokens.get(index + 1), Some(Token::Word(word)) if boolean_search_term(word).is_some() || importance_search_term(word).is_some() || search_phrase_word(word).is_some()),
            _ => true,
        })
    {
        return None;
    }
    let has_negated_phrase = tokens.iter().enumerate().any(|(index, token)| {
        matches!(token, Token::Not)
            && matches!(tokens.get(index + 1), Some(Token::Word(word)) if search_phrase_word(word).is_some())
    }) || tokens.iter().any(|token| {
        matches!(token, Token::Word(word) if word.strip_prefix('-').is_some_and(|word| search_phrase_word(word).is_some()))
    });
    if has_negated_phrase {
        for (index, token) in tokens.iter().enumerate() {
            let Token::Word(word) = token else { continue };
            let phrase = search_phrase_word(
                word.strip_prefix('+')
                    .or_else(|| word.strip_prefix('-'))
                    .unwrap_or(word),
            );
            let boolean = boolean_search_term(word)
                .or_else(|| word.strip_prefix('-').and_then(boolean_search_term));
            let importance = importance_search_term(word)
                .or_else(|| word.strip_prefix('-').and_then(importance_search_term));
            let negated_boolean =
                index > 0 && tokens[index - 1] == Token::Not && boolean_search_term(word).is_some();
            if phrase.is_none() && boolean.is_none() && importance.is_none() && !negated_boolean {
                // The candidate query is broadened to the whole mailbox for
                // negated phrases. Accept only predicates reproduced locally.
                return None;
            }
        }
    }
    let mut phrases = Vec::new();
    let mut phrase_positions = std::collections::HashMap::<String, usize>::new();
    for (index, token) in tokens.iter().enumerate() {
        let Token::Word(word) = token else { continue };
        if index > 0 && tokens[index - 1] == Token::Not {
            if let Some((property, value)) = search_phrase_word(word) {
                if let Some(previous) = phrase_positions.get(&property).copied() {
                    if !tokens[previous + 1..index].contains(&Token::And) {
                        return None;
                    }
                }
                phrase_positions.insert(property.clone(), index);
                phrases.push((format!("$not${property}"), value));
                continue;
            }
            if let Some((property, value)) = boolean_search_term(word) {
                phrases.push((format!("$not{property}"), value));
                continue;
            }
            if let Some((property, value)) = importance_search_term(word) {
                phrases.push((format!("$not${property}"), value));
                continue;
            }
        }
        if let Some(negative) = word.strip_prefix('-') {
            if let Some((property, value)) = search_phrase_word(negative) {
                if let Some(previous) = phrase_positions.get(&property).copied() {
                    if !tokens[previous + 1..index].contains(&Token::And) {
                        return None;
                    }
                }
                phrase_positions.insert(property.clone(), index);
                phrases.push((format!("$not${property}"), value));
                continue;
            }
        }
        let qualified_word = word.strip_prefix('+').unwrap_or(word);
        if let Some((property, value)) = search_phrase_word(qualified_word) {
            if let Some(previous) = phrase_positions.get(&property).copied() {
                // Repeating a property is safe only when an explicit AND
                // separates each phrase occurrence. Adjacent restrictions
                // on one property are implicitly OR-grouped by MS-KQL and
                // must not be post-filtered as an AND.
                if !tokens[previous + 1..index].contains(&Token::And) {
                    return None;
                }
            }
            phrase_positions.insert(property.clone(), index);
            phrases.push((property, value));
            continue;
        }
        if let Some(negative) = word.strip_prefix('-') {
            if search_phrase_word(negative).is_some() {
                return None;
            }
            if boolean_search_term(negative).is_some() {
                let (property, value) = boolean_search_term(negative)?;
                phrases.push((format!("$not{property}"), value));
                continue;
            }
            if let Some((property, value)) = importance_search_term(negative) {
                phrases.push((format!("$not${property}"), value));
                continue;
            }
        }
        if let Some((property, value)) = boolean_search_term(word) {
            phrases.push((property, value));
            continue;
        }
        if let Some((property, value)) = importance_search_term(word) {
            phrases.push((property, value));
            continue;
        }
        if let Some((property, _)) = word.split_once(':') {
            if phrases
                .iter()
                .any(|(phrase_property, _)| property.eq_ignore_ascii_case(phrase_property))
            {
                return None;
            }
        }
    }
    if phrases.is_empty() {
        return None;
    }
    for (phrase_property, _) in &phrases {
        for token in &tokens {
            let Token::Word(word) = token else { continue };
            if word
                .split_once(':')
                .is_some_and(|(property, _)| property.eq_ignore_ascii_case(phrase_property))
                && search_phrase_word(
                    word.strip_prefix('+')
                        .or_else(|| word.strip_prefix('-'))
                        .unwrap_or(word),
                )
                .is_none()
            {
                return None;
            }
        }
    }
    Some((PhraseMatch::All, phrases))
}

fn is_flat_or_expression(tokens: &[Token]) -> bool {
    let mut position = 0;
    let mut count = 0;
    while position < tokens.len() {
        if tokens.get(position) == Some(&Token::Not) {
            position += 1;
        }
        if !matches!(tokens.get(position), Some(Token::Word(_))) {
            return false;
        }
        count += 1;
        position += 1;
        if position < tokens.len() {
            if tokens.get(position) != Some(&Token::Or) {
                return false;
            }
            position += 1;
        }
    }
    count > 1
}

fn implicit_phrase_boolean_groups(tokens: &[Token]) -> Option<Vec<(String, String)>> {
    let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut group_positions = std::collections::HashMap::<String, usize>::new();
    let mut phrase_count = 0;
    let mut repeated_property = false;
    for token in tokens {
        let Token::Word(word) = token else {
            return None;
        };
        let (property, term) = if let Some((property, value)) =
            search_phrase_word(word.strip_prefix('+').unwrap_or(word))
        {
            phrase_count += 1;
            (property.clone(), (property, value))
        } else {
            let (term_property, value) = boolean_search_term(word)?;
            let property = term_property.strip_prefix('$')?.to_owned();
            (property, (term_property, value))
        };
        let position = if let Some(position) = group_positions.get(&property).copied() {
            repeated_property = true;
            position
        } else {
            let position = groups.len();
            groups.push((property.clone(), Vec::new()));
            group_positions.insert(property, position);
            position
        };
        groups[position].1.push(term);
    }
    if phrase_count == 0 || !repeated_property {
        return None;
    }
    let mut output = Vec::new();
    for (group_index, (_, terms)) in groups.into_iter().enumerate() {
        if terms.is_empty() {
            return None;
        }
        for (term_index, term) in terms.into_iter().enumerate() {
            output.push(term);
            if term_index > 0 {
                output.push(("$op$or".to_owned(), String::new()));
            }
        }
        if group_index > 0 {
            output.push(("$op$and".to_owned(), String::new()));
        }
    }
    Some(output)
}

fn flat_phrase_boolean_expression(tokens: &[Token]) -> Option<Vec<(String, String)>> {
    if tokens.iter().any(|token| {
        !matches!(
            token,
            Token::Word(_) | Token::And | Token::Or | Token::Not | Token::Left | Token::Right
        )
    }) {
        return None;
    }
    let mut output = Vec::new();
    let mut operators: Vec<(Token, bool)> = Vec::new();
    let mut position = 0;
    let mut expect_operand = true;
    let mut phrase_count = 0;
    let mut operator_count = 0;
    let mut properties = std::collections::HashSet::new();
    let mut phrase_positions = std::collections::HashMap::<String, usize>::new();
    while position < tokens.len() {
        if expect_operand {
            if tokens.get(position) == Some(&Token::Left) {
                operators.push((Token::Left, false));
                position += 1;
                continue;
            }
            let explicit_not = tokens.get(position) == Some(&Token::Not);
            if explicit_not {
                position += 1;
            }
            let Token::Word(word) = tokens.get(position)? else {
                return None;
            };
            if explicit_not {
                if word.starts_with(['+', '-']) {
                    return None;
                }
                let (property, value) =
                    boolean_search_term(word).or_else(|| importance_search_term(word))?;
                if !properties.insert(property.clone()) {
                    return None;
                }
                output.push((format!("$not{property}"), value));
            } else if let Some(negative) = word.strip_prefix('-') {
                if let Some((property, value)) = search_phrase_word(negative) {
                    if !register_expression_phrase(
                        tokens,
                        &mut properties,
                        &mut phrase_positions,
                        &property,
                        position,
                    ) {
                        return None;
                    }
                    output.push((format!("$not${property}"), value));
                    phrase_count += 1;
                } else {
                    let (property, value) = boolean_search_term(negative)
                        .or_else(|| importance_search_term(negative))?;
                    if !properties.insert(property.clone()) {
                        return None;
                    }
                    output.push((format!("$not{property}"), value));
                }
            } else {
                let positive = word.strip_prefix('+').unwrap_or(word);
                if let Some(phrase) = search_phrase_word(positive) {
                    if !register_expression_phrase(
                        tokens,
                        &mut properties,
                        &mut phrase_positions,
                        &phrase.0,
                        position,
                    ) {
                        return None;
                    }
                    output.push(phrase);
                    phrase_count += 1;
                } else {
                    let term =
                        boolean_search_term(word).or_else(|| importance_search_term(word))?;
                    if !properties.insert(term.0.clone()) {
                        return None;
                    }
                    output.push(term);
                }
            }
            position += 1;
            expect_operand = false;
        } else {
            let operator = tokens.get(position)?;
            if *operator == Token::Right {
                while operators
                    .last()
                    .is_some_and(|previous| previous.0 != Token::Left)
                {
                    let previous = operators.pop().unwrap().0;
                    output.push((
                        if previous == Token::And {
                            "$op$and"
                        } else {
                            "$op$or"
                        }
                        .to_owned(),
                        String::new(),
                    ));
                }
                if operators.pop() != Some((Token::Left, false)) {
                    return None;
                }
                position += 1;
                continue;
            }
            if !matches!(operator, Token::And | Token::Or) {
                if matches!(operator, Token::Word(_) | Token::Not | Token::Left) {
                    operators.push((Token::And, true));
                    operator_count += 1;
                    expect_operand = true;
                    continue;
                }
                return None;
            }
            while operators.last().is_some_and(|previous| {
                let (previous, implicit) = previous;
                let precedence = |operator: &Token, implicit: bool| match (operator, implicit) {
                    (Token::And, true) => 0,
                    (Token::Or, false) => 1,
                    (Token::And, false) => 2,
                    _ => -1,
                };
                precedence(previous, *implicit) >= precedence(operator, false)
            }) {
                let previous = operators.pop().unwrap().0;
                output.push((
                    if previous == Token::And {
                        "$op$and"
                    } else {
                        "$op$or"
                    }
                    .to_owned(),
                    String::new(),
                ));
            }
            operators.push((operator.clone(), false));
            operator_count += 1;
            position += 1;
            expect_operand = true;
        }
    }
    if expect_operand || phrase_count == 0 || operator_count == 0 {
        return None;
    }
    while let Some((operator, _)) = operators.pop() {
        if operator == Token::Left {
            return None;
        }
        output.push((
            if operator == Token::And {
                "$op$and"
            } else {
                "$op$or"
            }
            .to_owned(),
            String::new(),
        ));
    }
    Some(output)
}

fn register_expression_phrase(
    tokens: &[Token],
    properties: &mut std::collections::HashSet<String>,
    phrase_positions: &mut std::collections::HashMap<String, usize>,
    property: &str,
    position: usize,
) -> bool {
    if let Some(previous) = phrase_positions.get(property).copied() {
        let depth_at = |index: usize| {
            tokens[..index]
                .iter()
                .fold(0isize, |depth, token| match token {
                    Token::Left => depth + 1,
                    Token::Right => depth - 1,
                    _ => depth,
                })
        };
        let depth = depth_at(previous);
        if depth == 0
            || depth != depth_at(position)
            || !tokens[previous + 1..position]
                .iter()
                .all(|token| matches!(token, Token::Or | Token::Word(_) | Token::Not))
        {
            return false;
        }
    } else {
        properties.insert(property.to_owned());
    }
    phrase_positions.insert(property.to_owned(), position);
    true
}

fn search_phrase_word(word: &str) -> Option<(String, String)> {
    let (property, value) = word.split_once(':')?;
    if ![
        "subject",
        "body",
        "text",
        "from",
        "to",
        "cc",
        "bcc",
        "participants",
    ]
    .iter()
    .any(|allowed| property.eq_ignore_ascii_case(allowed))
        || !value.starts_with('"')
        || !value.ends_with('"')
        || value.len() < 4
        || value[1..value.len() - 1].contains('"')
        || value[1..value.len() - 1].contains('\\')
    {
        return None;
    }
    let phrase = &value[1..value.len() - 1];
    (phrase.split_whitespace().count() > 1)
        .then(|| (property.to_ascii_lowercase(), phrase.to_owned()))
}

fn boolean_search_term(word: &str) -> Option<(String, String)> {
    let word = word.strip_prefix('+').unwrap_or(word);
    let (property, operator, value) = if let Some((property, value)) = word.split_once(':') {
        (property, ":", value)
    } else if let Some((property, value)) = word.split_once("<>") {
        (property, "<>", value)
    } else if let Some((property, value)) = word.split_once('=') {
        (property, "=", value)
    } else {
        return None;
    };
    let property = property.to_ascii_lowercase();
    let enabled = match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => "true",
        "false" | "no" | "0" => "false",
        _ => return None,
    };
    matches!(
        property.as_str(),
        "isread" | "isflagged" | "hasattachment" | "hasattachments"
    )
    .then(|| {
        let property = if property == "hasattachments" {
            "hasattachment"
        } else {
            property.as_str()
        };
        (
            if operator == "<>" {
                format!("$not${property}")
            } else {
                format!("${property}")
            },
            enabled.to_owned(),
        )
    })
}

fn importance_search_term(word: &str) -> Option<(String, String)> {
    let (property, value) = word.strip_prefix('+').unwrap_or(word).split_once(':')?;
    if !property.eq_ignore_ascii_case("importance") {
        return None;
    }
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .to_ascii_lowercase();
    let value = match value.as_str() {
        "low" => "low",
        "normal" | "medium" => "normal",
        "high" => "high",
        _ => return None,
    };
    Some(("$importance".to_owned(), value.to_owned()))
}

pub(super) fn subject_has_phrase(subject: &str, phrase: &str) -> bool {
    let words = |text: &str| {
        text.split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    let subject = words(subject);
    let phrase = words(phrase);
    !phrase.is_empty() && subject.windows(phrase.len()).any(|window| window == phrase)
}

fn tokenize(input: &str) -> Result<Vec<Token>, &'static str> {
    let mut chars = input.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '(' {
            tokens.push(Token::Left);
            continue;
        }
        if c == ')' {
            tokens.push(Token::Right);
            continue;
        }
        if c == ',' {
            tokens.push(Token::Comma);
            continue;
        }
        let mut value = String::new();
        let mut quoted = false;
        let mut in_quote = false;
        let mut next = Some(c);
        while let Some(c) = next {
            match c {
                '"' => {
                    quoted = true;
                    in_quote = !in_quote;
                    value.push(c);
                }
                '\\' if in_quote => {
                    value.push(c);
                    value.push(chars.next().ok_or("unfinished escape")?);
                }

                _ => value.push(c),
            }
            if !in_quote
                && chars
                    .peek()
                    .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | ')' | ','))
            {
                break;
            }
            next = chars.next();
        }
        if in_quote {
            return Err("unclosed search quote");
        }
        let token = match value.as_str() {
            "AND" if !quoted => Token::And,
            "OR" if !quoted => Token::Or,
            "NOT" if !quoted => Token::Not,
            "WORDS" if !quoted => Token::Words,
            "ALL" if !quoted => Token::All,
            "ANY" if !quoted => Token::Any,
            "NONE" if !quoted => Token::None,
            "NEAR" | "ONEAR" | "XRANK" if !quoted => {
                return Err("unsupported search operator");
            }
            _ => Token::Word(value),
        };
        tokens.push(token);
        if tokens.len() > 1024 {
            return Err("search expression is too complex");
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}
impl Parser {
    // Implicit operators have lower precedence than explicit AND/OR.
    // Repeated adjacent property restrictions combine with OR [MS-KQL] 2.2.4.
    fn implicit(&mut self, depth: usize) -> Result<Value, &'static str> {
        let mut groups: Vec<(Option<String>, Value)> = Vec::new();
        while self.position < self.tokens.len() && self.tokens[self.position] != Token::Right {
            let start = self.position;
            let value = self.or(depth)?;
            let property = match &self.tokens[start..self.position] {
                [Token::Word(word)] if !word.starts_with('"') => word
                    .split_once(':')
                    .map(|(key, _)| key.to_ascii_lowercase()),
                _ => None,
            };
            if let Some((_, previous)) = groups
                .iter_mut()
                .find(|(key, _)| property.is_some() && *key == property)
            {
                *previous = combine("OR", previous.take(), value);
            } else {
                groups.push((property, value));
            }
        }
        let mut values = groups.into_iter().map(|(_, value)| value);
        let first = values.next().ok_or("empty search expression")?;
        Ok(values.fold(first, |left, right| combine("AND", left, right)))
    }

    fn or(&mut self, depth: usize) -> Result<Value, &'static str> {
        let mut value = self.and(depth)?;
        while self.tokens.get(self.position) == Some(&Token::Or) {
            self.position += 1;
            value = combine("OR", value, self.and(depth)?);
        }
        Ok(value)
    }

    fn and(&mut self, depth: usize) -> Result<Value, &'static str> {
        let mut value = self.unary(depth)?;
        while self.tokens.get(self.position) == Some(&Token::And) {
            self.position += 1;
            value = combine("AND", value, self.unary(depth)?);
        }
        Ok(value)
    }

    fn unary(&mut self, depth: usize) -> Result<Value, &'static str> {
        if depth > 32 {
            return Err("search nesting is too deep");
        }
        let token = self
            .tokens
            .get(self.position)
            .cloned()
            .ok_or("missing search operand")?;
        self.position += 1;
        match token {
            Token::Not => Ok(json!({"operator": "NOT", "conditions": [self.unary(depth + 1)?]})),
            Token::Words => self.words_operator(),
            Token::All => self.operator_terms("AND", false, false),
            Token::Any => self.operator_terms("OR", false, false),
            Token::None => Ok(json!({
                "operator":"NOT",
                "conditions":[self.operator_terms("OR", false, false)?]
            })),
            Token::Left => {
                let value = self.implicit(depth + 1)?;
                if self.tokens.get(self.position) != Some(&Token::Right) {
                    return Err("unclosed search group");
                }
                self.position += 1;
                Ok(value)
            }
            Token::Word(word) => word_condition(&word),
            _ => Err("unexpected search operator"),
        }
    }

    fn words_operator(&mut self) -> Result<Value, &'static str> {
        self.operator_terms("OR", true, true)
    }

    fn operator_terms(
        &mut self,
        operator: &str,
        ignore_qualifiers: bool,
        allow_commas: bool,
    ) -> Result<Value, &'static str> {
        if self.tokens.get(self.position) != Some(&Token::Left) {
            return Err("operator requires parentheses");
        }
        self.position += 1;
        let mut terms = Vec::new();
        loop {
            match self.tokens.get(self.position).cloned() {
                Some(Token::Right) => break,
                Some(Token::Comma) if allow_commas => self.position += 1,
                Some(Token::Comma) => return Err("unexpected operator-list separator"),
                Some(Token::Word(word)) => {
                    self.position += 1;
                    let term = if ignore_qualifiers {
                        word.trim_start_matches(['+', '-'])
                    } else {
                        word.as_str()
                    };
                    if term.is_empty()
                        || (!ignore_qualifiers && term.starts_with(['+', '-']))
                        || (!term.starts_with('"')
                            && (comparison_index(term).is_some() || term.contains(':')))
                        || (term.starts_with('"') && term.chars().any(char::is_whitespace))
                    {
                        return Err("operator accepts only searchable text terms");
                    }
                    terms.push(condition("text", term)?);
                }
                _ => return Err("invalid WORDS term list"),
            }
        }
        if terms.is_empty() {
            return Err("operator requires at least one term");
        }
        self.position += 1;
        let mut terms = terms.into_iter();
        let first = terms.next().unwrap();
        Ok(terms.fold(first, |left, right| combine(operator, left, right)))
    }
}

fn word_condition(word: &str) -> Result<Value, &'static str> {
    if let Some(term) = word.strip_prefix('+') {
        if term.is_empty() || term.starts_with(['+', '-']) {
            return Err("invalid search qualifier");
        }
        // MS-KQL ignores '+' when it qualifies a property restriction; under
        // this parser's AND default it does not change a positive text term.
        return word_condition(term);
    }
    if let Some(term) = word.strip_prefix('-') {
        if term.is_empty() || term.starts_with(['+', '-']) {
            return Err("invalid search qualifier");
        }
        return Ok(json!({"operator":"NOT", "conditions":[word_condition(term)?]}));
    }
    if word.starts_with('"') {
        if word.chars().any(char::is_whitespace) {
            return Err("unqualified phrase search is unavailable");
        }
        return condition("text", word);
    }
    if let Some(index) = comparison_index(word) {
        let property = &word[..index];
        let rest = &word[index..];
        let width = if rest.starts_with(">=") || rest.starts_with("<=") || rest.starts_with("<>") {
            2
        } else {
            1
        };
        let (operator, value) = rest.split_at(width);
        if property.eq_ignore_ascii_case("size") {
            return size_condition(operator, value);
        }
        if property.eq_ignore_ascii_case("received") {
            return received_condition(operator, value);
        }
        if is_boolean_property(property) {
            if !matches!(operator, "=" | "<>") {
                return Err("invalid boolean comparison");
            }
            let filter = condition(&property.to_ascii_lowercase(), value)?;
            return Ok(if operator == "<>" {
                json!({"operator":"NOT", "conditions":[filter]})
            } else {
                filter
            });
        }
        if known_property(property) {
            return Err("unsupported search comparison");
        }
        return free_text_property(property, value);
    }
    if let Some((property, value)) = word.split_once(':') {
        if value.is_empty() {
            // MS-KQL only recognizes a property restriction when name,
            // operator, and value are contiguous. A space after the operator
            // makes `name:` an ordinary text token.
            return condition("text", word);
        }
        condition(&property.to_ascii_lowercase(), value)
    } else {
        condition("text", word)
    }
}

fn comparison_index(word: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in word.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if !quoted && matches!(ch, '<' | '>' | '=') {
            return Some(index);
        }
    }
    None
}

fn combine(operator: &str, mut left: Value, right: Value) -> Value {
    if left["operator"] == operator {
        left["conditions"].as_array_mut().unwrap().push(right);
        left
    } else {
        json!({"operator": operator, "conditions": [left, right]})
    }
}

fn size_condition(operator: &str, value: &str) -> Result<Value, &'static str> {
    if operator == ":" {
        if let Some((start, end)) = value.split_once("..") {
            let start = start.parse::<u64>().map_err(|_| "invalid size range")?;
            let end = end.parse::<u64>().map_err(|_| "invalid size range")?;
            if start > end || end >= 9_007_199_254_740_991 {
                return Err("invalid size range");
            }
            return Ok(json!({"minSize":start,"maxSize":end + 1}));
        }
    }
    let size = value.parse::<u64>().map_err(|_| "invalid size")?;
    // JMAP UnsignedInt is limited to the interoperable JSON integer range.
    if size >= 9_007_199_254_740_991 {
        return Err("size is too large");
    }
    Ok(match operator {
        ":" | "=" => json!({"minSize": size, "maxSize": size + 1}),
        ">=" => json!({"minSize": size}),
        ">" => json!({"minSize": size + 1}),
        "<" => json!({"maxSize": size}),
        "<=" => json!({"maxSize": size + 1}),
        "<>" => json!({"operator": "NOT", "conditions": [{"minSize": size, "maxSize": size + 1}]}),
        _ => return Err("invalid size comparison"),
    })
}

fn received_condition(operator: &str, value: &str) -> Result<Value, &'static str> {
    received_condition_at(operator, value, chrono::Utc::now().date_naive())
}

fn received_condition_at(
    operator: &str,
    value: &str,
    today: chrono::NaiveDate,
) -> Result<Value, &'static str> {
    use chrono::{Datelike, Months};
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    if operator == ":" {
        if let Some((start, end)) = value.split_once("..") {
            return Ok(json!({"operator":"AND", "conditions":[
                received_condition(">=", start)?, received_condition("<=", end)?
            ]}));
        }
    }
    let (start, end) = match value.to_ascii_lowercase().as_str() {
        "today" => (today, today.succ_opt().ok_or("date out of range")?),
        "yesterday" => (today.pred_opt().ok_or("date out of range")?, today),
        "tomorrow" => {
            let start = today.succ_opt().ok_or("date out of range")?;
            (start, start.succ_opt().ok_or("date out of range")?)
        }
        "this week" => {
            let start = today
                .checked_sub_days(chrono::Days::new(
                    today.weekday().num_days_from_monday() as u64
                ))
                .ok_or("date out of range")?;
            (
                start,
                start
                    .checked_add_days(chrono::Days::new(7))
                    .ok_or("date out of range")?,
            )
        }
        "monday" | "tuesday" | "wednesday" | "thursday" | "friday" | "saturday" | "sunday" => {
            let week_start = today
                .checked_sub_days(chrono::Days::new(
                    today.weekday().num_days_from_monday() as u64
                ))
                .ok_or("date out of range")?;
            let day_offset = match value.to_ascii_lowercase().as_str() {
                "monday" => 0,
                "tuesday" => 1,
                "wednesday" => 2,
                "thursday" => 3,
                "friday" => 4,
                "saturday" => 5,
                "sunday" => 6,
                _ => unreachable!(),
            };
            let start = week_start
                .checked_add_days(chrono::Days::new(day_offset))
                .ok_or("date out of range")?;
            (start, start.succ_opt().ok_or("date out of range")?)
        }
        "last week" => {
            let this_week = today
                .checked_sub_days(chrono::Days::new(
                    today.weekday().num_days_from_monday() as u64
                ))
                .ok_or("date out of range")?;
            (
                this_week
                    .checked_sub_days(chrono::Days::new(7))
                    .ok_or("date out of range")?,
                this_week,
            )
        }
        "next month" => {
            let start = today
                .with_day(1)
                .ok_or("date out of range")?
                .checked_add_months(Months::new(1))
                .ok_or("date out of range")?;
            (
                start,
                start
                    .checked_add_months(Months::new(1))
                    .ok_or("date out of range")?,
            )
        }
        "this month" => {
            let start = today.with_day(1).ok_or("date out of range")?;
            (
                start,
                start
                    .checked_add_months(Months::new(1))
                    .ok_or("date out of range")?,
            )
        }
        "last month" | "past month" => {
            let end = today.with_day(1).ok_or("date out of range")?;
            (
                end.checked_sub_months(Months::new(1))
                    .ok_or("date out of range")?,
                end,
            )
        }
        "this year" => {
            let start = today.with_ordinal(1).ok_or("date out of range")?;
            (
                start,
                start
                    .checked_add_months(Months::new(12))
                    .ok_or("date out of range")?,
            )
        }
        "last year" => {
            let end = today.with_ordinal(1).ok_or("date out of range")?;
            (
                end.checked_sub_months(Months::new(12))
                    .ok_or("date out of range")?,
                end,
            )
        }
        _ => {
            let day = if value.len() > 10 {
                if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
                    date.with_timezone(&chrono::Utc).date_naive()
                } else {
                    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                        .map_err(|_| "invalid received date")?
                        .date()
                }
            } else {
                if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
                    date
                } else if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%m/%d/%Y") {
                    date
                } else {
                    let (month, day) = value.split_once('/').ok_or("invalid received date")?;
                    chrono::NaiveDate::from_ymd_opt(
                        today.year(),
                        month.parse().map_err(|_| "invalid received date")?,
                        day.parse().map_err(|_| "invalid received date")?,
                    )
                    .ok_or("invalid received date")?
                }
            };
            (day, day.succ_opt().ok_or("date out of range")?)
        }
    };
    if start.year() < 1 || end.year() > 9999 {
        return Err("date out of range");
    }
    let start = format!("{}T00:00:00Z", start.format("%Y-%m-%d"));
    let end = format!("{}T00:00:00Z", end.format("%Y-%m-%d"));
    // Some Stalwart versions treat "after" as exclusive. Negating the
    // exclusive "before" filter preserves the inclusive boundary.
    let at_or_after = |date: &str| json!({"operator":"NOT", "conditions":[{"before":date}]});
    let within = json!({"operator":"AND", "conditions":[at_or_after(&start), {"before":end}]});
    Ok(match operator {
        ":" | "=" => within,
        ">=" => at_or_after(&start),
        ">" => at_or_after(&end),
        "<" => json!({"before": start}),
        "<=" => json!({"before": end}),
        "<>" => json!({"operator":"NOT", "conditions":[within]}),
        _ => return Err("invalid date comparison"),
    })
}

fn condition(property: &str, value: &str) -> Result<Value, &'static str> {
    if value.is_empty() || value == "\"\"" || value.contains('*') || value.contains('?') {
        return Err("unsupported or empty search value");
    }
    if property == "received" {
        return received_condition(":", value);
    }
    if property == "importance" {
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value)
            .to_ascii_lowercase();
        let value = match value.as_str() {
            "low" => "low",
            "normal" | "medium" => "normal",
            "high" => "high",
            _ => return Err("invalid importance search value"),
        };
        return Ok(json!({"header": ["Importance", value]}));
    }
    if property == "kind" {
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        if value.eq_ignore_ascii_case("email") {
            // Mailbox Find already queries Email objects exclusively.
            return Ok(json!({}));
        }
        return Err("unsupported search kind");
    }
    // Stalwart's native index does not preserve phrase order. Positive
    // subject/body/text phrases in an AND-only expression are post-filtered.
    if value.starts_with('"') && value.chars().any(char::is_whitespace) {
        if let Some((phrase_property, phrase)) = search_phrase_word(&format!("{property}:{value}"))
        {
            if phrase_property == "participants" {
                return Ok(json!({"operator":"OR", "conditions":[
                    {"from": phrase}, {"to": phrase}, {"cc": phrase}, {"bcc": phrase}
                ]}));
            }
            return Ok(json!({phrase_property: phrase}));
        }
        return Err("phrase search is unavailable");
    }
    match property {
        "size" => size_condition(":", value),
        "text" | "subject" | "body" | "from" | "to" | "cc" | "bcc" => Ok(json!({property: value})),
        "participants" => Ok(json!({"operator": "OR", "conditions": [
            {"from": value}, {"to": value}, {"cc": value}, {"bcc": value}]})),
        "isread" | "isflagged" => {
            let enabled = match value.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => true,
                "false" | "no" | "0" => false,
                _ => return Err("invalid boolean search value"),
            };
            let keyword = if property == "isread" {
                "$seen"
            } else {
                "$flagged"
            };
            Ok(if enabled {
                json!({"hasKeyword": keyword})
            } else {
                json!({"notKeyword": keyword})
            })
        }
        "hasattachment" | "hasattachments" => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Ok(json!({"hasAttachment": true})),
            "false" | "no" | "0" => Ok(json!({"hasAttachment": false})),
            _ => Err("invalid attachment search value"),
        },
        _ if known_property(property) => Err("unsupported search property"),
        _ => free_text_property(property, value),
    }
}

fn known_property(property: &str) -> bool {
    [
        "text",
        "subject",
        "body",
        "from",
        "to",
        "cc",
        "bcc",
        "participants",
        "size",
        "received",
        "sent",
        "importance",
        "kind",
        "isread",
        "isflagged",
        "hasattachment",
        "hasattachments",
    ]
    .iter()
    .any(|known| property.eq_ignore_ascii_case(known))
}

fn is_boolean_property(property: &str) -> bool {
    ["isread", "isflagged", "hasattachment", "hasattachments"]
        .iter()
        .any(|known| property.eq_ignore_ascii_case(known))
}

fn free_text_property(property: &str, value: &str) -> Result<Value, &'static str> {
    let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        let inner = &value[1..value.len() - 1];
        if inner.chars().any(char::is_whitespace) || inner.contains('"') {
            return Err("unsupported property phrase");
        }
        inner
    } else if value.contains('"') {
        return Err("unsupported property value");
    } else {
        value
    };
    if property.is_empty() || value.is_empty() {
        return Err("unsupported property value");
    }
    Ok(json!({"text": format!("{property} {value}")}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positive_conjunctive_subject_phrases_are_candidates_for_exact_post_filtering() {
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta""#),
            Some((
                PhraseMatch::All,
                vec![("subject".to_owned(), "alpha beta".to_owned())]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"from:"Alice Example""#),
            Some((
                PhraseMatch::All,
                vec![("from".to_owned(), "Alice Example".to_owned())]
            ))
        );
        assert_eq!(
            to_filter(r#"from:"Alice Example""#).unwrap(),
            json!({"from":"Alice Example"})
        );
        assert_eq!(
            to_filter(r#"participants:"Alice Example""#).unwrap(),
            json!({"operator":"OR", "conditions":[
                {"from":"Alice Example"}, {"to":"Alice Example"},
                {"cc":"Alice Example"}, {"bcc":"Alice Example"}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" AND isread:true"#),
            Some((
                PhraseMatch::All,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR isread=true"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR isread<>false"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$not$isread".to_owned(), "false".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" AND isread:true"#).unwrap(),
            json!({"operator":"AND", "conditions":[
                {"subject":"alpha beta"}, {"hasKeyword":"$seen"}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" AND WORDS(alpha beta)"#),
            Some((
                PhraseMatch::All,
                vec![("subject".to_owned(), "alpha beta".to_owned())]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" AND WORDS(alpha beta)"#).unwrap(),
            json!({"operator":"AND", "conditions":[
                {"subject":"alpha beta"},
                {"operator":"OR", "conditions":[{"text":"alpha"}, {"text":"beta"}]}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" AND NOT isread:true"#),
            Some((
                PhraseMatch::All,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$not$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" AND NOT isread:true"#).unwrap(),
            json!({"operator":"AND", "conditions":[
                {"subject":"alpha beta"},
                {"operator":"NOT", "conditions":[{"hasKeyword":"$seen"}]}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" AND -isflagged:true"#),
            Some((
                PhraseMatch::All,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$not$isflagged".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta""#).unwrap(),
            json!({"subject":"alpha beta"})
        );
        assert_eq!(
            phrases_for_postfilter(r#"NOT subject:"alpha beta""#),
            Some((
                PhraseMatch::None,
                vec![("subject".to_owned(), "alpha beta".to_owned())]
            ))
        );
        assert_eq!(
            to_filter(r#"NOT subject:"alpha beta""#).unwrap(),
            json!({"operator":"NOT", "conditions":[{"subject":"alpha beta"}]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"-body:"alpha beta""#),
            Some((
                PhraseMatch::None,
                vec![("body".to_owned(), "alpha beta".to_owned())]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR isread:true"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR NOT isread:true"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$not$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"isread:false OR subject:"alpha beta""#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("$isread".to_owned(), "false".to_owned()),
                    ("subject".to_owned(), "alpha beta".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" OR isread:true"#).unwrap(),
            json!({"operator":"OR", "conditions":[
                {"subject":"alpha beta"}, {"hasKeyword":"$seen"}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR isflagged:yes"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isflagged".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"hasattachments:no OR subject:"alpha beta""#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("$hasattachment".to_owned(), "false".to_owned()),
                    ("subject".to_owned(), "alpha beta".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR isread:true OR isflagged:yes"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned()),
                    ("$isflagged".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR -isread:true"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$not$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(
                r#"subject:"alpha beta" OR subject:"gamma delta" OR isread:true"#
            ),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("subject".to_owned(), "gamma delta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR (isread:true AND isflagged:true)"#),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned()),
                    ("$isflagged".to_owned(), "true".to_owned()),
                    ("$op$and".to_owned(), String::new()),
                    ("$op$or".to_owned(), String::new())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" isread:true OR isflagged:true"#),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned()),
                    ("$isflagged".to_owned(), "true".to_owned()),
                    ("$op$or".to_owned(), String::new()),
                    ("$op$and".to_owned(), String::new())
                ]
            ))
        );
        for query in [
            r#"subject:"alpha beta" subject:gamma"#,
            r#"subject:"alpha beta" OR subject:gamma"#,
        ] {
            assert!(phrases_for_postfilter(query).is_none(), "{query}");
            assert!(to_filter(query).is_err(), "{query}");
        }
        assert!(phrases_for_postfilter(r#"text:"alpha beta" OR from:bob"#).is_none());
    }

    #[test]
    fn positive_conjunctive_body_and_text_phrases_use_exact_postfiltering() {
        assert_eq!(
            phrases_for_postfilter(r#"body:"alpha beta" AND isread:true"#),
            Some((
                PhraseMatch::All,
                vec![
                    ("body".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"body:"alpha beta" AND isread:true"#).unwrap(),
            json!({"operator":"AND", "conditions":[
                {"body":"alpha beta"}, {"hasKeyword":"$seen"}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"text:"alpha beta""#),
            Some((
                PhraseMatch::All,
                vec![("text".to_owned(), "alpha beta".to_owned())]
            ))
        );
        for query in [
            r#"body:"alpha beta" body:gamma"#,
            r#"text:"alpha beta" text:gamma"#,
        ] {
            assert!(phrases_for_postfilter(query).is_none(), "{query}");
            assert!(to_filter(query).is_err(), "{query}");
        }
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" AND body:"gamma delta""#),
            Some((
                PhraseMatch::All,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("body".to_owned(), "gamma delta".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" AND body:"gamma delta""#).unwrap(),
            json!({"operator":"AND", "conditions":[
                {"subject":"alpha beta"}, {"body":"gamma delta"}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" AND subject:"gamma delta""#),
            Some((
                PhraseMatch::All,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("subject".to_owned(), "gamma delta".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" AND subject:"gamma delta""#).unwrap(),
            json!({"operator":"AND", "conditions":[
                {"subject":"alpha beta"}, {"subject":"gamma delta"}
            ]})
        );
        let query = r#"subject:"alpha beta" -subject:"gamma delta""#;
        assert!(phrases_for_postfilter(query).is_none(), "{query}");
        assert!(to_filter(query).is_err(), "{query}");
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" subject:"gamma delta""#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("subject".to_owned(), "gamma delta".to_owned())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" subject:"gamma delta""#).unwrap(),
            json!({"operator":"OR", "conditions":[
                {"subject":"alpha beta"}, {"subject":"gamma delta"}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" subject:"gamma delta" isread:true"#),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("subject".to_owned(), "gamma delta".to_owned()),
                    ("$op$or".to_owned(), String::new()),
                    ("$isread".to_owned(), "true".to_owned()),
                    ("$op$and".to_owned(), String::new())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" body:"gamma delta" OR isread:true"#),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("body".to_owned(), "gamma delta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned()),
                    ("$op$or".to_owned(), String::new()),
                    ("$op$and".to_owned(), String::new())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" AND NOT body:"gamma delta""#),
            Some((
                PhraseMatch::All,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$not$body".to_owned(), "gamma delta".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR body:"gamma delta""#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("body".to_owned(), "gamma delta".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"NOT subject:"alpha beta" OR subject:"gamma delta""#),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("$not$subject".to_owned(), "alpha beta".to_owned()),
                    ("subject".to_owned(), "gamma delta".to_owned()),
                    ("$op$or".to_owned(), String::new())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(
                r#"(subject:"alpha beta" OR subject:"gamma delta") AND isread:true"#
            ),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("subject".to_owned(), "gamma delta".to_owned()),
                    ("$op$or".to_owned(), String::new()),
                    ("$isread".to_owned(), "true".to_owned()),
                    ("$op$and".to_owned(), String::new())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(
                r#"(subject:"alpha beta" OR isread:true OR subject:"gamma delta") AND isflagged:true"#
            ),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned()),
                    ("$op$or".to_owned(), String::new()),
                    ("subject".to_owned(), "gamma delta".to_owned()),
                    ("$op$or".to_owned(), String::new()),
                    ("$isflagged".to_owned(), "true".to_owned()),
                    ("$op$and".to_owned(), String::new())
                ]
            ))
        );
        assert_eq!(
            to_filter(r#"subject:"alpha beta" OR body:"gamma delta""#).unwrap(),
            json!({"operator":"OR", "conditions":[
                {"subject":"alpha beta"}, {"body":"gamma delta"}
            ]})
        );
        assert_eq!(
            phrases_for_postfilter(r#"NOT subject:"alpha beta" AND isread:true"#),
            Some((
                PhraseMatch::All,
                vec![
                    ("$not$subject".to_owned(), "alpha beta".to_owned()),
                    ("$isread".to_owned(), "true".to_owned())
                ]
            ))
        );
    }

    #[test]
    fn exact_subject_phrase_requires_adjacent_words_without_case_sensitivity() {
        assert!(subject_has_phrase("Re: Alpha, BETA! Update", "alpha beta"));
        assert!(!subject_has_phrase("Beta Alpha", "alpha beta"));
        assert!(!subject_has_phrase("Alpha unrelated Beta", "alpha beta"));
    }

    #[test]
    fn translates_property_restrictions_and_boolean_precedence() {
        assert_eq!(
            to_filter("subject: mobile").unwrap(),
            json!({"operator":"AND", "conditions":[
                {"text":"subject:"}, {"text":"mobile"}
            ]})
        );
        assert_eq!(
            to_filter("from:a OR from:b AND NOT isread:true").unwrap(),
            json!({"operator":"OR", "conditions":[
                {"from":"a"}, {"operator":"AND", "conditions":[{"from":"b"}, {"operator":"NOT", "conditions":[{"hasKeyword":"$seen"}]}]}
            ]})
        );
        assert_eq!(
            to_filter("from:a from:b subject:\"mobile\"").unwrap(),
            json!({"operator":"AND", "conditions":[
                {"operator":"OR", "conditions":[{"from":"a"}, {"from":"b"}]}, {"subject":"\"mobile\""}
            ]})
        );
        assert_eq!(
            to_filter("(subject:a OR subject:b) isflagged:true").unwrap(),
            json!({"operator":"AND", "conditions":[
                {"operator":"OR", "conditions":[{"subject":"a"}, {"subject":"b"}]}, {"hasKeyword":"$flagged"}
            ]})
        );
    }
    #[test]
    fn translates_inclusive_size_and_received_ranges() {
        assert_eq!(
            to_filter("size:10..20").unwrap(),
            json!({"minSize":10,"maxSize":21})
        );
        assert!(to_filter("size:20..10").is_err());
        assert!(to_filter("size:1..").is_err());
        assert_eq!(
            to_filter("received:2024-02-29..2024-03-01").unwrap(),
            json!({
                "operator":"AND", "conditions":[
                    {"operator":"NOT", "conditions":[{"before":"2024-02-29T00:00:00Z"}]},
                    {"before":"2024-03-02T00:00:00Z"}
                ]
            })
        );
        assert!(to_filter("received:2024-02-29..").is_err());
        assert_eq!(
            to_filter("received:2024-02-29T12:34:56").unwrap(),
            to_filter("received:2024-02-29").unwrap()
        );
        assert_eq!(
            to_filter("received:2024-02-29T12:34:56.123").unwrap(),
            to_filter("received:2024-02-29").unwrap()
        );
        assert!(to_filter("received:2024-02-29T25:00:00").is_err());
    }

    #[test]
    fn quoted_terms_are_not_property_restrictions_or_operators() {
        assert_eq!(
            to_filter(r#"subject:"a=b""#).unwrap(),
            json!({"subject": "\"a=b\""})
        );
        assert_eq!(
            to_filter("\"from:a\"").unwrap(),
            json!({"text": "\"from:a\""})
        );
        assert_eq!(
            to_filter("subject: \"mobile\"").unwrap(),
            json!({"operator":"AND", "conditions":[
                {"text":"subject:"}, {"text":"\"mobile\""}
            ]})
        );
        assert_eq!(to_filter("\"AND\"").unwrap(), json!({"text": "\"AND\""}));
        assert_eq!(
            to_filter(r#"subject:"a\"b""#).unwrap(),
            json!({"subject": r#""a\"b""#})
        );
    }

    #[test]
    fn unknown_properties_fall_back_to_text_tokens() {
        assert_eq!(
            to_filter("unknownproperty:word").unwrap(),
            json!({"text":"unknownproperty word"})
        );
        assert_eq!(
            to_filter("unknownproperty>7").unwrap(),
            json!({"text":"unknownproperty 7"})
        );
        assert_eq!(
            to_filter("unknownproperty:\"word\"").unwrap(),
            json!({"text":"unknownproperty word"})
        );
        assert!(to_filter("unknownproperty:\"two words\"").is_err());
    }

    #[test]
    fn kql_qualifiers_map_to_required_and_excluded_terms() {
        assert_eq!(
            to_filter("+subject:mobile").unwrap(),
            json!({"subject":"mobile"})
        );
        assert_eq!(
            to_filter("-from:alice@example.com").unwrap(),
            json!({"operator":"NOT", "conditions":[{"from":"alice@example.com"}]})
        );
        assert_eq!(
            to_filter("urgent -subject:spam").unwrap(),
            json!({"operator":"AND", "conditions":[
                {"text":"urgent"},
                {"operator":"NOT", "conditions":[{"subject":"spam"}]}
            ]})
        );
        assert!(to_filter("-").is_err());
        assert!(to_filter("++term").is_err());
    }

    #[test]
    fn boolean_property_comparisons_preserve_equality_and_inequality() {
        for (query, expected) in [
            ("isread=true", json!({"hasKeyword":"$seen"})),
            ("isread=false", json!({"notKeyword":"$seen"})),
            ("isflagged=1", json!({"hasKeyword":"$flagged"})),
            ("hasattachment=yes", json!({"hasAttachment":true})),
            ("hasattachments=no", json!({"hasAttachment":false})),
            (
                "hasattachment<>true",
                json!({"operator":"NOT", "conditions":[{"hasAttachment":true}]}),
            ),
            (
                "isread<>false",
                json!({"operator":"NOT", "conditions":[{"notKeyword":"$seen"}]}),
            ),
        ] {
            assert_eq!(to_filter(query).unwrap(), expected, "{query}");
        }
        assert!(to_filter("isread>1").is_err());
        assert!(to_filter("isread=maybe").is_err());
    }

    #[test]
    fn importance_property_searches_the_standard_message_header() {
        for (query, value) in [
            ("importance:low", "low"),
            ("importance:normal", "normal"),
            ("importance:medium", "normal"),
            ("importance:high", "high"),
            ("importance:\"HIGH\"", "high"),
        ] {
            assert_eq!(
                to_filter(query).unwrap(),
                json!({"header": ["Importance", value]}),
                "{query}"
            );
        }
        assert!(to_filter("importance:urgent").is_err());
        assert!(to_filter("importance=high").is_err());
        assert_eq!(
            phrases_for_postfilter(r#"subject:"alpha beta" OR importance:high"#),
            Some((
                PhraseMatch::Any,
                vec![
                    ("subject".to_owned(), "alpha beta".to_owned()),
                    ("$importance".to_owned(), "high".to_owned())
                ]
            ))
        );
        assert_eq!(
            phrases_for_postfilter(r#"NOT subject:"alpha beta" OR importance:high"#),
            Some((
                PhraseMatch::Expression,
                vec![
                    ("$not$subject".to_owned(), "alpha beta".to_owned()),
                    ("$importance".to_owned(), "high".to_owned()),
                    ("$op$or".to_owned(), String::new())
                ]
            ))
        );
    }

    #[test]
    fn email_kind_is_satisfied_by_mailbox_find_scope() {
        assert_eq!(to_filter("kind:email").unwrap(), json!({}));
        assert_eq!(
            to_filter("subject:invoice kind:EMAIL").unwrap(),
            json!({
                "operator":"AND",
                "conditions":[{"subject":"invoice"}, {}]
            })
        );
        assert!(to_filter("kind:contacts").is_err());
        assert!(to_filter("kind=email").is_err());
    }

    #[test]
    fn words_operator_searches_for_any_listed_text_term() {
        for query in ["WORDS(TV, Television)", "WORDS(TV Television)"] {
            assert_eq!(
                to_filter(query).unwrap(),
                json!({"operator":"OR", "conditions":[
                    {"text":"TV"}, {"text":"Television"}
                ]})
            );
        }
        assert_eq!(
            to_filter("WORDS(+TV, -Television)").unwrap(),
            json!({"operator":"OR", "conditions":[
                {"text":"TV"}, {"text":"Television"}
            ]})
        );
        assert!(to_filter("WORDS()").is_err());
        assert!(to_filter("WORDS(subject:TV)").is_err());
        assert!(to_filter("WORDS(\"two words\")").is_err());
    }

    #[test]
    fn all_any_and_none_operators_map_to_boolean_text_filters() {
        assert_eq!(
            to_filter("ALL(cat dog fox)").unwrap(),
            json!({"operator":"AND", "conditions":[
                {"text":"cat"}, {"text":"dog"}, {"text":"fox"}
            ]})
        );
        assert_eq!(
            to_filter("ANY(cat dog)").unwrap(),
            json!({"operator":"OR", "conditions":[
                {"text":"cat"}, {"text":"dog"}
            ]})
        );
        assert_eq!(
            to_filter("NONE(cat dog)").unwrap(),
            json!({"operator":"NOT", "conditions":[{"operator":"OR", "conditions":[
                {"text":"cat"}, {"text":"dog"}
            ]}]})
        );
        assert!(to_filter("ALL(cat, dog)").is_err());
        assert!(to_filter("ANY(subject:cat)").is_err());
        assert!(to_filter("NONE()").is_err());
    }

    #[test]
    fn translates_size_boundaries_without_off_by_one_errors() {
        for (query, filter) in [
            ("size:7", json!({"minSize":7,"maxSize":8})),
            ("size=7", json!({"minSize":7,"maxSize":8})),
            ("size>7", json!({"minSize":8})),
            ("size>=7", json!({"minSize":7})),
            ("size<7", json!({"maxSize":7})),
            ("size<=7", json!({"maxSize":8})),
            (
                "size<>7",
                json!({"operator":"NOT","conditions":[{"minSize":7,"maxSize":8}]}),
            ),
        ] {
            assert_eq!(to_filter(query).unwrap(), filter);
        }
        for query in [
            "size>",
            "size>=-1",
            "size>1kb",
            "size=9007199254740991",
            "subject=x",
        ] {
            assert!(to_filter(query).is_err(), "{query}");
        }
    }

    #[test]
    fn received_date_filters_use_utc_day_boundaries() {
        let not_before = |day: &str| json!({"operator":"NOT","conditions":[{"before":day}]});
        let start = "2026-02-28T00:00:00Z";
        let end = "2026-03-01T00:00:00Z";
        let within = json!({"operator":"AND","conditions":[not_before(start),{"before":end}]});
        for (query, expected) in [
            ("received:2026-02-28", within.clone()),
            ("received>2024-02-29", not_before("2024-03-01T00:00:00Z")),
            ("received>=2026-01-01", not_before("2026-01-01T00:00:00Z")),
            (
                "received<2026-01-01",
                json!({"before":"2026-01-01T00:00:00Z"}),
            ),
            (
                "received<=2026-01-01",
                json!({"before":"2026-01-02T00:00:00Z"}),
            ),
            (
                "received<>2026-02-28",
                json!({"operator":"NOT","conditions":[within]}),
            ),
        ] {
            assert_eq!(to_filter(query).unwrap(), expected);
        }
        assert_eq!(
            to_filter("received:2026-02-28T15:45:00Z").unwrap(),
            to_filter("received:2026-02-28").unwrap()
        );
        assert_eq!(
            to_filter("received:2024-03-01T00:30:00+02:00").unwrap(),
            to_filter("received:2024-02-29").unwrap()
        );
        assert_eq!(
            to_filter("received:2024-02-29T23:30:00-02:00").unwrap(),
            to_filter("received:2024-03-01").unwrap()
        );
        assert!(to_filter("received:2026-02-30").is_err());
        assert!(to_filter("sent:2026-01-01").is_err());
        assert!(to_filter(r#"received:"this month""#).is_ok());
    }

    #[test]
    fn received_relative_dates_cover_tomorrow_weeks_and_months() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        let cases = [
            ("tomorrow", "2026-11-25T00:00:00Z", "2026-11-26T00:00:00Z"),
            ("last week", "2026-11-16T00:00:00Z", "2026-11-23T00:00:00Z"),
            ("next month", "2026-12-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            ("past month", "2026-10-01T00:00:00Z", "2026-11-01T00:00:00Z"),
        ];
        for (value, start, end) in cases {
            assert_eq!(
                received_condition_at(":", value, today).unwrap(),
                json!({"operator":"AND", "conditions":[
                    {"operator":"NOT", "conditions":[{"before":start}]},
                    {"before":end}
                ]}),
                "{value}"
            );
        }
    }

    #[test]
    fn received_dates_accept_kql_year_and_current_year_numeric_forms() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        let expected = json!({"operator":"AND", "conditions":[
            {"operator":"NOT", "conditions":[{"before":"2026-11-24T00:00:00Z"}]},
            {"before":"2026-11-25T00:00:00Z"}
        ]});
        for value in ["11/24/2026", "11/24"] {
            assert_eq!(
                received_condition_at(":", value, today).unwrap(),
                expected,
                "{value}"
            );
        }
        assert!(received_condition_at(":", "13/40/2026", today).is_err());
    }

    #[test]
    fn received_weekday_search_uses_the_current_monday_based_week() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        for (value, start, end) in [
            ("monday", "2026-11-23T00:00:00Z", "2026-11-24T00:00:00Z"),
            ("TUESDAY", "2026-11-24T00:00:00Z", "2026-11-25T00:00:00Z"),
            ("Sunday", "2026-11-29T00:00:00Z", "2026-11-30T00:00:00Z"),
        ] {
            assert_eq!(
                received_condition_at(":", value, today).unwrap(),
                json!({"operator":"AND", "conditions":[
                    {"operator":"NOT", "conditions":[{"before":start}]},
                    {"before":end}
                ]}),
                "{value}"
            );
        }
    }

    #[test]
    fn rejects_queries_that_cannot_be_translated() {
        for query in [
            "",
            "AND",
            "a OR",
            "(a",
            "a)",
            "sent:2026-01-01",
            "foo*",
            "\"hello world\"",
            "a NEAR b",
            "subject:\"unfinished",
        ] {
            assert!(to_filter(query).is_err(), "{query}");
        }
        assert_eq!(
            to_filter("subject:\"hello world\"").unwrap(),
            json!({"subject":"hello world"})
        );
        assert_eq!(to_filter("from:").unwrap(), json!({"text":"from:"}));
        assert!(to_filter(&format!("{}a{}", "(".repeat(40), ")".repeat(40))).is_err());
    }
}
