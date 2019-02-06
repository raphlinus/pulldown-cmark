// Tests for HTML spec.

extern crate pulldown_cmark;

#[test]
fn html_test_1() {
    let original = r##"Little header

<script type="text/js">
function some_func() {
console.log("teeeest");
}


function another_func() {
console.log("fooooo");
}
</script>"##;
    let expected = r##"<p>Little header</p>
<script type="text/js">
function some_func() {
console.log("teeeest");
}


function another_func() {
console.log("fooooo");
}
</script>"##;

    use pulldown_cmark::{html, Parser};

    let mut s = String::new();

    let p = Parser::new(&original);
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}

#[test]
fn html_test_2() {
    let original = r##"Little header

<script
type="text/js">
function some_func() {
console.log("teeeest");
}


function another_func() {
console.log("fooooo");
}
</script>"##;
    let expected = r##"<p>Little header</p>
<script
type="text/js">
function some_func() {
console.log("teeeest");
}


function another_func() {
console.log("fooooo");
}
</script>"##;

    use pulldown_cmark::{html, Parser};

    let mut s = String::new();

    let p = Parser::new(&original);
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}

#[test]
fn html_test_3() {
    let original = r##"Little header

<?
<div></div>
<p>Useless</p>
?>"##;
    let expected = r##"<p>Little header</p>
<?
<div></div>
<p>Useless</p>
?>"##;

    use pulldown_cmark::{html, Parser};

    let mut s = String::new();

    let p = Parser::new(&original);
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}

#[test]
fn html_test_4() {
    let original = r##"Little header

<!--
<div></div>
<p>Useless</p>
-->"##;
    let expected = r##"<p>Little header</p>
<!--
<div></div>
<p>Useless</p>
-->"##;

    use pulldown_cmark::{html, Parser};

    let mut s = String::new();

    let p = Parser::new(&original);
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}

#[test]
fn html_test_5() {
    let original = r##"Little header

<![CDATA[
<div></div>
<p>Useless</p>
]]>"##;
    let expected = r##"<p>Little header</p>
<![CDATA[
<div></div>
<p>Useless</p>
]]>"##;

    use pulldown_cmark::{html, Parser};

    let mut s = String::new();

    let p = Parser::new(&original);
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}

#[test]
fn html_test_6() {
    let original = r##"Little header

<!X
Some things are here...
>"##;
    let expected = r##"<p>Little header</p>
<!X
Some things are here...
>"##;

    use pulldown_cmark::{html, Parser};

    let mut s = String::new();

    let p = Parser::new(&original);
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}

#[test]
fn html_test_7() {
    let original = r##"Little header
-----------

<script>
function some_func() {
console.log("teeeest");
}


function another_func() {
console.log("fooooo");
}
</script>"##;
    let expected = r##"<h2>Little header</h2>
<script>
function some_func() {
console.log("teeeest");
}


function another_func() {
console.log("fooooo");
}
</script>"##;

    use pulldown_cmark::{html, Parser};

    let mut s = String::new();

    let p = Parser::new(&original);
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}

// TODO: add broken link callback feature
/*
#[test]
fn html_test_broken_callback() {
    let original = r##"[foo],
[bar],
[baz],

   [baz]: https://example.org
"##;

    let expected = r##"<p><a href="https://replaced.example.org" title="some title">foo</a>,
[bar],
<a href="https://example.org">baz</a>,</p>
"##;

    use pulldown_cmark::{Options, Parser, html};

    let mut s = String::new();

    let callback = |reference: &str, _normalized: &str| -> Option<(String, String)> {
        if reference == "foo" || reference == "baz" {
            Some(("https://replaced.example.org".into(), "some title".into()))
        } else {
            None
        }
    };

    let p = Parser::new_with_broken_link_callback(&original, Options::empty(), Some(&callback));
    html::push_html(&mut s, p);

    assert_eq!(expected, s);
}
*/
