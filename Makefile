PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
MANDIR ?= $(PREFIX)/share/man/man1
DOCDIR ?= $(PREFIX)/share/doc/ftty
TARGET ?= $(shell [ -f ftty ] && echo ftty || echo target/release/ftty)

all: $(TARGET)

target/release/ftty: FORCE
	cargo build --release

doc:
	@date=$$(LC_ALL=C git log -1 --format=%cd --date=format:'%B %Y' doc/ftty.1.md 2>/dev/null || LC_ALL=C date +"%B %Y"); \
	version=$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1); \
	pandoc -s -t man doc/ftty.1.md -M date="$$date" -M footer="ftty $$version" -o doc/ftty.1

install: $(TARGET)
	install -Dm755 $(TARGET) $(DESTDIR)$(BINDIR)/ftty
	install -Dm644 doc/ftty.1 $(DESTDIR)$(MANDIR)/ftty.1
	install -Dm644 README.md $(DESTDIR)$(DOCDIR)/README.md
	install -Dm644 LICENSE $(DESTDIR)$(PREFIX)/share/licenses/ftty/LICENSE

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/ftty
	rm -f $(DESTDIR)$(MANDIR)/ftty.1
	rm -rf $(DESTDIR)$(DOCDIR)
	rm -f $(DESTDIR)$(PREFIX)/share/licenses/ftty/LICENSE

FORCE:

.PHONY: all doc install uninstall FORCE
