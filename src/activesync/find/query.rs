use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    Left,
    Right,
    And,
    Or,
    Not,
}

pub(super) fn to_filter(input: &str) -> Result<Value, &'static str> {
    let tokens = tokenize(input)?;
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
        if matches!(c, '+' | '-') {
            return Err("unsupported search operator");
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
                '<' | '>' | '=' if !in_quote => return Err("unsupported search comparison"),
                _ => value.push(c),
            }
            if !in_quote
                && chars
                    .peek()
                    .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | ')'))
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
            "NEAR" | "ONEAR" | "XRANK" | "WORDS" if !quoted => {
                return Err("unsupported search operator")
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
                [Token::Word(word), Token::Word(_)] if word.ends_with(':') => {
                    Some(word[..word.len() - 1].to_ascii_lowercase())
                }
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
            Token::Left => {
                let value = self.implicit(depth + 1)?;
                if self.tokens.get(self.position) != Some(&Token::Right) {
                    return Err("unclosed search group");
                }
                self.position += 1;
                Ok(value)
            }
            Token::Word(word) => {
                if word.starts_with('"') {
                    return condition("text", &word);
                }
                if let Some((property, value)) = word.split_once(':') {
                    let value = if value.is_empty() {
                        let Some(Token::Word(value)) = self.tokens.get(self.position) else {
                            return Err("missing property value");
                        };
                        self.position += 1;
                        value.as_str()
                    } else {
                        value
                    };
                    condition(&property.to_ascii_lowercase(), value)
                } else {
                    condition("text", &word)
                }
            }
            _ => Err("unexpected search operator"),
        }
    }
}

fn combine(operator: &str, mut left: Value, right: Value) -> Value {
    if left["operator"] == operator {
        left["conditions"].as_array_mut().unwrap().push(right);
        left
    } else {
        json!({"operator": operator, "conditions": [left, right]})
    }
}

fn condition(property: &str, value: &str) -> Result<Value, &'static str> {
    if value.is_empty() || value == "\"\"" || value.contains('*') || value.contains('?') {
        return Err("unsupported or empty search value");
    }
    // Stalwart 0.16.19's native index does not preserve phrase order.
    if value.starts_with('"') && value.chars().any(char::is_whitespace) {
        return Err("phrase search is unavailable");
    }
    match property {
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
        _ => Err("unsupported search property"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translates_property_restrictions_and_boolean_precedence() {
        assert_eq!(
            to_filter("subject: mobile").unwrap(),
            json!({"subject": "mobile"})
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
    fn quoted_terms_are_not_property_restrictions_or_operators() {
        assert_eq!(
            to_filter("\"from:a\"").unwrap(),
            json!({"text": "\"from:a\""})
        );
        assert_eq!(
            to_filter("subject: \"mobile\"").unwrap(),
            json!({"subject": "\"mobile\""})
        );
        assert_eq!(to_filter("\"AND\"").unwrap(), json!({"text": "\"AND\""}));
        assert_eq!(
            to_filter(r#"subject:"a\"b""#).unwrap(),
            json!({"subject": r#""a\"b""#})
        );
    }

    #[test]
    fn rejects_queries_that_cannot_be_translated() {
        for query in [
            "",
            "AND",
            "a OR",
            "(a",
            "a)",
            "from:",
            "sent:2026-01-01",
            "foo*",
            "subject:\"hello world\"",
            "\"hello world\"",
            "-from:a",
            "+subject:a",
            "size>=1024",
            "received<2026-01-01",
            "a NEAR b",
            "subject:\"unfinished",
        ] {
            assert!(to_filter(query).is_err(), "{query}");
        }
        assert!(to_filter(&format!("{}a{}", "(".repeat(40), ")".repeat(40))).is_err());
    }
}
