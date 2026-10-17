; test_compare.asm
; Checks Ne / Le / Ge on Int, Float and Str.
; Expected output, in order:
; 5 ne 3   -> true
; 5 ne 5   -> false
; 3 le 5   -> true
; 5 le 5   -> true
; 6 le 5   -> false
; 5 ge 3   -> true
; 5 ge 5   -> true
; 3 ge 5   -> false
; 1.5 le 2.5 -> true
; "abc" ne "abd" -> true

push 5
push 3
ne
call emit_value 1
pop

push 5
push 5
ne
call emit_value 1
pop

push 3
push 5
le
call emit_value 1
pop

push 5
push 5
le
call emit_value 1
pop

push 6
push 5
le
call emit_value 1
pop

push 5
push 3
ge
call emit_value 1
pop

push 5
push 5
ge
call emit_value 1
pop

push 3
push 5
ge
call emit_value 1
pop

push 1.5
push 2.5
le
call emit_value 1
pop

push "abc"
push "abd"
ne
call emit_value 1
pop

halt


; Ordinary output function built from generic runtime calls.
emit_value:
store 0
call_native io::stdout
load 0
call_native bytes::render
store 1
load 1
push 0
load 1
call_native bytes::len
call_native io::write_all
dup
is_type error
jnz emit_done
pop
call_native io::stdout
call_native io::flush
emit_done:
ret
