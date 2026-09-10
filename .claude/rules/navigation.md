---
paths:
  - "src/analysis/locator.rs"
  - "src/analysis/symbols.rs"
  - "src/analysis/hover.rs"
  - "src/analysis/render.rs"
  - "src/analysis/requires.rs"
  - "src/analysis/search.rs"
---

# Navigation, outlines and rendering

## Tiers and confidence

- **Hover and go-to-definition answer three tiers, and the card says which.** Between "rubydex
  named the receiver" and "matched on the method name alone" is a rung for a receiver ya-lsp typed
  itself, from an RBS return type or an assignment. Both requests go through
  `locator::resolve_typed`, which is why they cannot disagree; `references`, the type hierarchy and
  `rename` deliberately do not. `types.md` has the whole of it, including why a *local* assigned a
  literal carries no footnote while an instance variable does.
- **A hover footnote is about ya-lsp's confidence, never about the code.** Three kinds: nothing (the
  code names the type), what was derived and where from, and the guess. The offset-to-line
  conversion for an assignment happens in `analysis::mod`, where the document is; `hover::markdown`
  renders the line it is handed and knows nothing about encodings.
- **A hover card is the answer, then what ya-lsp knows about the answer.** Fence, rule, prose, then
  footnotes — one italic line each, at the bottom. `hover::footnote` is the one place the shape
  lives, because it was two: a guessed single match carried the caveat as a trailing italic and a
  guessed *list* carried the same sentence inline, in bold, at the top, after an em dash. Four cards
  are pinned whole in `analysis::tests` — a core method, a stdlib method, a reopened class, a
  name-only match — the way `ANCESTRY` pins the first ten completions, because composition drifts
  while every part still passes a `contains`.
- **Hover cards are pinned side by side, in one file, whole.**
  `every_hover_card_in_one_file_drawn_side_by_side` renders every construct in `GALLERY` with its
  card under it. Before it existed, each construct had a `contains` somewhere and no two cards were
  read next to each other — which is how `class << Book` came to sit above a
  `private Shelf::Book#hide`: `attached_name` read the attached class out of rubydex's
  `Shelf::Book::<Book>`, where it is spelled unqualified, and the one test covering it used a
  *top-level* module, where qualified and unqualified are the same string. **Take the part before
  the `::<`, never the part inside it.** A new card or construct goes in that fixture.

## rubydex's spellings

- **rubydex invents constant references you must not follow.** Every call with an implicit or
  constant receiver gets a fabricated reference to `<Foo>` so the singleton class can be resolved.
  `Person.new` therefore has two references over the same six bytes, and for an *implicit* receiver
  the invented one spans the **whole call** — so `alias_method :a, :b` carries a reference resolving
  to `class << self`. Angle brackets are rubydex's only spelling for singleton names;
  `analysis::locator` drops references starting with `<`.
- **rubydex records an anonymous `*`, `**` or `&` under the sigil itself**, not under an empty name,
  so `render`'s `sigil` writes it once. Prepending unconditionally spelled `def f(*, **, &)` as
  `(**, ****, &&)`, which `.new` hovers made visible on every constructor taking one.
- **A qualified name has to be put back together to be looked up.** `constant_path` walks the chain
  of `ParentScope`s rubydex interned the reference as, because the graph holds only the last
  segment's string on each `Name`. `resolve_outwards` is the lexical half of Ruby's constant lookup,
  the same pair `types`' guess rung uses.
- **rubydex is what makes "is `self` a class object here?" one question.** A receiverless call
  written as a statement of a class or module body records the *singleton* as its receiver; the same
  call inside a `def` records the class itself. Both halves below turn on `attached_class`
  answering, so neither needs a syntactic test and neither can disagree with the other.

## `Foo.new` and concerns

- **`Foo.new` is answered with `Foo#initialize`, and the redirect is marked.** `Class#new` is the
  exact answer and a useless one — on a Rails app **every** `.new` site hovered as
  `Class#new(*args, **kwargs, &block)` in `core/class.rbs`. `locator::constructor` steps off it
  through the *attached* class, which is the singleton's `owner_id` and not a name taken apart. It
  stands aside twice: a hand-written `def self.new` is what the call really reaches (rbs declares one
  for `Struct`), and `BasicObject#initialize` is what every object inherits rather than a constructor
  anyone wrote, so a class with neither keeps `Class#new`. Both guards ask the *owner* of what was
  found.
- **`Resolution::redirected` exists because `references` must not take the redirect.**
  `def initialize` is not a declaration of `new`, and with `includeDeclaration` it was listed as one
  for every class whose constructor is in the user's code. Navigation and
  `completion::precise_call` want the redirect — a constructor's keyword arguments really are what
  `Foo.new(` takes. Anything new reading `Resolution` has to decide which it is.
- **A concern's `ClassMethods` is on the singleton of every including class, and the edge is walked
  rather than declared.** `ActiveSupport::Concern#append_features` ends with
  `base.extend const_get(:ClassMethods)`; Rails writes the `include` statically and never writes that
  `extend`, so ya-lsp's singleton lookup correctly found nothing and `validates`, `scope`,
  `belongs_to` and `has_many` in a model body answered on the name rung — **thousands of sites
  across five applications**, over every distinct `ClassMethods` name Rails writes. Walked in
  `locator::extended_by_a_concern`, not written into RBS, because the class it would have to be
  declared on is `ActiveRecord::Base`, which lives in a gem's `lib/` and is not a document any
  generator may write into.
- **The gate is the nested `module ClassMethods`, deliberately not `extend ActiveSupport::Concern`.**
  Of the modules across six applications holding one, only about half extend
  `ActiveSupport::Concern`; the rest hand-roll `def self.included(base); base.extend(ClassMethods)`,
  write `Order.singleton_class.prepend self::ClassMethods`, or reopen a module Rails already made a
  concern — **every one of them really installs it, and an `ActiveSupport` test would decline
  many**. The
  nested module is what the spellings have in common, and nobody writes that name for another
  purpose. A `ClassMethods` that is a constant rather than a module declines in `own_member`.
- **What is taken from the module is what it declares itself, never what its ancestors declare.**
  `extend M` really installs the instance methods of `M`'s ancestors, so an ancestor walk is the
  right reading of Ruby and the wrong reading of the graph: rubydex records an `include` written
  **inside a `def`** as a mixin of the enclosing namespace, which is exactly how Rails writes the
  ones reaching a `ClassMethods` module — `def has_secure_password; include ActiveModel::Validations;
  end` means the *record's* ancestors at the moment the macro is called. Over Rails' five core gems
  and the six applications, **the `module ClassMethods` blocks hold a couple of mixins in the module
  body and rather more inside a `def`**, and every one of the latter means the class the macro was
  called on. The applications write
  none of either. Following them made `Story.valid?` — which raises in Ruby — resolve precisely to
  `ActiveModel::Validations#valid?`. The cost is named rather than estimated:
  `ActiveRecord::Callbacks::ClassMethods`' `define_model_callbacks` and two helper-path methods of
  `AbstractController::Helpers::ClassMethods`.
- **It is asked after the ordinary ancestor search and only when that found nothing** — the rule
  `resolve_typed` already holds for a derived receiver: a worse answer may never displace a better
  one. A class writing its own `def self.validates` keeps it.
- **`textDocument/completion` follows the same walk, and `locator::extended_class_methods` makes it
  one gate rather than two.** Resolution takes the first module answering one member; completion
  collects every member of all of them. Both read the same list in the same order, so a change to
  what counts as a concern cannot land on one half only. `completion.md` has the collecting side.

## Narrowing the name-based fallback

- **The fallback is filtered when the receiver is a class object, and the argument is the ancestor
  chain, not a heuristic.** `locator::reachable_on_a_class_object` drops a candidate whose owner is a
  `class`: what a class object answers is its own singleton chain — the singleton classes of its
  ancestors, plus whatever is `extend`ed onto it, plus the instance methods of `Class`, `Module`,
  `Object` and `Kernel` — and rubydex puts every one of those in the singleton's *own* ancestors,
  which the search above already walked without finding the member. So a surviving class-owned
  candidate is provably unreachable, while a module-owned one is exactly what must stay: a concern's
  `ClassMethods` is a module's **instance** method. The list is narrowed, never emptied.
- **Swept twice, because the demand sits in two kinds of position: better in bulk, worse nowhere.**
  Most of the gain is at receiverless class-body calls, the rest at `.member`. Every lateral move is
  `list -> name`, never the reverse: the rank cannot see a long candidate list collapsing to the
  right single answer, because that is the same rung. **Every per-corpus zero is a bundle that is
  not installed** — a corpus pinning an `activerecord` the measuring machine does not have moves
  nothing — so every number is a floor, and the corpora with installed bundles are the shape of it.
  **What still moves under a missing bundle is the application's own concern**: a corpus with no
  `Gemfile.lock` still moves on its own `scope`s and its own `ClassMethods` modules.
- **A receiverless call in a class body resolves precisely, and the Rails macros are the known
  exception — the *fallback* is what is wrong there, not the scoping.** Measured against a real
  application's own models with its bundle indexed: a `def self.` on the same class, one on a
  superclass, and a
  method of a module the class statically `extend`s all answer *resolved*, and completion offers
  exactly those. `validates`, `has_many`, `belongs_to` and `scope` do not, because the edge that
  would put them on the singleton is runtime reflection no file states. So the name rung firing there
  is the **fallback working as designed**, and "the name search is not scoped" describes a symptom of
  a missing edge. The half belonging here is the second route: the candidate list is not filtered by
  what such a call could reach, so `scope` offers a long list headed by
  `ActionDispatch::Routing::Mapper::Scoping#scope`, and an **instance** method of an unrelated class
  can never be what a class body's bare call reaches.

## Spans, outlines and rendering

- **`selectionRange` must sit inside `range`, and Prism's error recovery hands out pairs that do
  not.** `locator::spans` is the one place reconciling them, for `DocumentSymbol` and `LocationLink`
  alike — VS Code enforces the first by *throwing*, dropping the entire outline rather than the one
  bad symbol. A bare `def` at the end of a line recovers into a node whose location is the three
  keyword bytes and whose name location is the whitespace after them; a sweep over five files typed
  character by character, at every intermediate state, found that shape and no other. Never build the pair by hand
  from `offset()` and `name_offset()`.
- **A definition with no name in it is not an outline entry** — the visible half of the same
  recovery, a blank row while `def` is being typed. `is_outline_worthy` checks it because
  `document_symbols` already builds every name; `locator::locate` deliberately does not, since a
  string lookup per definition would land on every hover and completion for a transient that only
  shows when the cursor is parked on the whitespace.
- **A definition matches only its name span, never its body.** `Definition::name_offset` is `None`
  for constants, `attr_*` and aliases — there `offset()` is already just the name. Widening this to
  the body makes hover fire over whitespace.
- **`locator::site` is the single point a declaration becomes a place**, which is why generated
  declarations are translated back there. Goto-definition, `references`, the symbol picker and the
  type hierarchy all reach a file through it, so one lookup covers all of them — and a generated
  definition with nothing recorded about where it came from answers `None` rather than handing out a
  link into a document the editor cannot open. `synthesized.md` has the rest.
- **Which definition of a declaration a *list* points at is `locator::preferred_definition`, one
  decision.** A class reopened two hundred times has two hundred definitions, and every list
  mentioning it once must pick the same one, or the class opens in `app/models` from the outline and
  in whichever gem reopened it from the type hierarchy. `locator::declared_in` is the other half —
  whether *any* of them is the user's — which both the symbol picker and the subtype list rank by.
  Neither belongs to the feature that wanted it first.
- **How a construct is spelled for a human lives in `render`, and is shared.** `split_qualified`
  (symbol lists) and `qualified_name` (hover) both go through `singleton_parts`, and
  `symbols::kind_of` is shared with `search`. A symbol reading differently in the outline and the
  picker is a bug, not a style choice.
- **`render::signature_label` writes the label and the spans in one pass, and the offsets are UTF-16
  code units.** `signatureHelp` highlights a parameter by handing the client a pair of offsets into
  the label, so the function that writes the string says where it wrote each piece. LSP's other
  spelling — the parameter as a substring to search for — mis-highlights the moment a label holds the
  same token twice, and `def each(key, value = key)` already does. UTF-16 because that is what a
  client indexes the label by; the protocol ties `Position` to the negotiated encoding and says
  nothing about these, and `def приветствие(имя)` is legal Ruby.

## RDoc and markdown

- **Nothing raw-HTML may reach a `MarkupContent`, and nothing inside code may be touched.** RDoc
  lifted Ruby's documentation out of the C source and left the HTML in — `<code>` spans throughout
  the vendored signatures, plus `<em>`, `<strong>`, `<tt>`, `<b>`, `<i>` — and a client renders a hover
  as markdown, which strips them: `<code><=></code>` arrived as a bare `<=>`, and prose merely
  looking like a tag (`<vowel>`, `<rhs>`, `<main>`, a whole `<html>` example) vanished with
  everything up to the next `>`. `render::to_markdown` converts what is markup and escapes what is
  not; RDoc's `rdoc-ref:` links — `core/` alone is full of them, every one pointing into a tree the
  editor has never seen — keep their words and lose the link. It skips fenced blocks, four-space and
  tab-indented blocks, and backtick spans: what is in them is Ruby, and a backslash in front of
  `Hash<Symbol, untyped>` is a visible bug where the old behaviour was a silent one.
- **RDoc is a markup, and `to_markdown` was only reading the HTML RDoc had already turned into.**
  Ruby's vendored signatures carry generated markup; a *gem's own source* is RDoc, and none of it was
  read. Six spellings ship, and the cheapest matters most: **RDoc's verbatim block is two spaces and
  markdown's is four**, which is the whole of "examples are plain text".
- **The same two spaces mean two different things, and what opened above them decides which.** A line
  under a labelled list item (`[+:autosave+]`, or `autosave::`) is the item's description and must
  stay prose; a line under a paragraph is verbatim. Read as verbatim, **every** option description
  in `has_many`'s comment became a code block. `list_item` is therefore recognised *before* the
  indentation is read.
- **`+word+` and `<tt>word</tt>` are the same markup.** RDoc's rule is that the `+` opens at a
  non-word boundary and closes before one, and nothing inside is whitespace — which is what keeps
  `1 + 2` and `a+b` prose.
- **`:nodoc:` is RDoc saying there is nothing here**, and a card with the word in it is worse than no
  card: a reader takes something in a card as an answer. It is `is_directive`'s, not
  `to_markdown`'s, so it only leads — prose above a `:nodoc:` is still documentation.
- Measured on `ActiveRecord::Associations::ClassMethods#has_many` and its neighbours: **most probed
  cards change**, examples become indented code, the option list becomes a markdown list, and every
  `+:autosave+` becomes a code span.

## Three ways a lookup goes wrong

- **A `def` written inside a block is recorded as a private method of `Object`, with nothing to tell
  it from a true top-level one.** rubydex's nesting stack holds lexical scopes, `Class.new` /
  `Module.new` owners and methods; a `describe "x" do` pushes none of the three. No flag, no variant,
  no nesting id — so declining to treat such a `def` as `Object`'s is **closed at the graph level**,
  and would be wrong anyway, since a top-level `def` really *is* a private method of `Object` and
  really is reachable from every class body.
- **What was left is the order, and it was wrong rather than approximate.** A module `extend`ed onto
  a class object sits above `Class`, `Module` and `Object` in the singleton chain, so the concern
  edge was always meant to be reached first. `resolve_call` now asks it **before** keeping a hit on
  one of `Object`, `Kernel` or `BasicObject`, and keeps the root answer wherever the edge has
  nothing. On a real corpus that is `validate` in a model body going from
  `private Object#validate(config)` — whose definitions are all RSpec spec files — to
  `ActiveModel::Validations::ClassMethods#validate`. **A hit on a root carries almost no
  information**, which is why it is the one hit worth asking a second question about.
- **`extend` is not unread; what it loses is one narrow case.** rubydex indexes `extend` and attaches
  it to the singleton class exactly as Ruby does. Four probes over one workspace isolated it:

  | written | resolves |
  | --- | --- |
  | Ruby: `extend Flat`, `extend Ns::Fmt`, `include Ns::Fmt` | yes |
  | RBS: `extend Flat`, `include Ns::Fmt` | yes |
  | **RBS: `extend Ns::Fmt`** | **no** |

  `stdlib/securerandom/0/securerandom.rbs` writes `extend Random::Formatter` — **most `extend`s in
  the vendored signatures are qualified**, `CGI::Util`, `Minitest::Spec::DSL` and a run of
  `OpenSSL::Marshal::ClassMethods` among them.
- **So `extends_written_on` is a repair, not a second walk, and it cannot double-count.** It is
  reached from `extended_by_a_concern`, which resolution asks only after the ordinary ancestor search
  came back empty, and from completion's `Extended`, which drops every member the ordinary walk
  already offers. An `extend` rubydex did linearize is found by the search and never gets there. Only
  where the receiver's singleton chain really passes through that declaration's singleton — the
  attached declaration and the classes above it. An `extend` written in an *included module* lands on
  that module's own singleton and never on the includer's.
