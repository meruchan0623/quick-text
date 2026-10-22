#!/usr/bin/env bash
# Bash twin of scripts/check-hook-lints.ps1 (S-02 / B-02). Keep the two in sync.
#
# The hook and RawInput callback modules must open (after comments and other inner
# attributes) with inner attributes that leave all of these at deny or forbid:
#
#   clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing
#
# CI runs clippy with -A clippy::unwrap_used -A clippy::expect_used until 0.5, so the
# module-level #![deny(...)] is the only thing enforcing the rule there. The attribute may
# be wrapped over several lines, split into several attributes, or use forbid. A later
# allow/warn/expect of one of the lints in the header cancels the deny. #[allow] on single
# items further down and blocking locks are NOT checked; review those in the PR.
#
# Usage: bash scripts/check-hook-lints.sh [file ...]
#   Relative paths are resolved against the repository root. The default list is the two
#   files below. A missing file is an error. Needs perl. Exit code 0 = pass, 1 = failure.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$#" -eq 0 ]; then
  set -- crates/platform/src/windows/hook.rs crates/platform/src/windows/rawinput.rs
fi

status=0

for file in "$@"; do
  if [ ! -f "$file" ]; then
    echo "FAIL: $file: file not found ($(pwd)/$file). The hook and RawInput callback modules" >&2
    echo "      are created by the platform split (S-23, B-02). If they were renamed or moved," >&2
    echo "      update the default list in scripts/check-hook-lints.ps1 and" >&2
    echo "      scripts/check-hook-lints.sh, and the call in .github/workflows/ci.yml." >&2
    status=1
    continue
  fi

  if ! perl -e '
    use strict;
    use warnings;

    my ($file) = @ARGV;
    open(my $fh, "<:raw", $file) or die "cannot open $file: $!";
    local $/;
    my $text = <$fh>;
    close($fh);
    $text =~ s/\A\xEF\xBB\xBF//;

    my @required = qw(clippy::unwrap_used clippy::expect_used clippy::panic clippy::indexing_slicing);
    my %level;

    # Walk the leading whitespace, comments and inner attributes; stop at the first item.
    pos($text) = 0;
    while (pos($text) < length($text)) {
      next if $text =~ /\G\s+/gc;
      next if $text =~ /\G\/\/[^\r\n]*/gc;
      next if $text =~ /\G\/\*.*?\*\//gcs;
      if ($text =~ /\G(#!\[(?:[^\[\]]|\[[^\]]*\])*\])/gc) {
        my $attr = $1;
        if ($attr =~ /\A#!\[\s*(allow|warn|deny|forbid|expect)\s*\((.*)\)\s*\]\z/s) {
          my ($lvl, $list) = ($1, $2);
          $list =~ s/reason\s*=\s*"[^"]*"//g;
          $list =~ s{//[^\r\n]*}{}g;
          for my $item (split /,/, $list) {
            $item =~ s/\A\s+|\s+\z//g;
            next if $item eq "";
            next if defined($level{$item}) && $level{$item} eq "forbid";
            $level{$item} = $lvl;
          }
        }
        next;
      }
      last;
    }

    my @bad;
    for my $lint (@required) {
      if (!exists $level{$lint}) {
        push @bad, "$lint (missing)";
      } elsif ($level{$lint} ne "deny" && $level{$lint} ne "forbid") {
        push @bad, "$lint (set to $level{$lint})";
      }
    }

    if (@bad) {
      print STDERR "FAIL: $file: the module header must contain #![deny(" . join(", ", @required) . ")] before any item; not enforced: " . join(", ", @bad) . "\n";
      exit 1;
    }
    print "ok:   $file enforces " . join(", ", @required) . "\n";
  ' "$file"; then
    status=1
  fi
done

exit "$status"
