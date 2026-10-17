; test_closures.asm
; Classic closure test: a make_counter pattern using an explicit
; Cell (reference capture). Call the same increment three times on
; one counter, then create a second independent counter.
;
; Expected output:
; 1
; 2
; 3
; 11

push 0
cell
store 0          ; local 0 = Cell(0) — shared mutable counter cell

load 0
closure @increment 1   ; capture the Cell by reference (not its value)
store 1          ; local 1 = Closure(increment, [Cell(0)])

load 1
callv 0
call emit_value 1            ; 1
pop
load 1
callv 0
call emit_value 1            ; 2
pop
load 1
callv 0
call emit_value 1            ; 3
pop

; second independent counter: its own Cell and Closure
push 10
cell
store 2
load 2
closure @increment 1
store 3
load 3
callv 0
call emit_value 1            ; 11  (10 + 1, one call)
pop

halt

.function increment 0 1
increment:
loadcap 0        ; captured Cell
dup
cellget          ; current value
push 1
add
cellset          ; write the updated value back and return the cell
cellget          ; read the updated value as the function result
ret


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
