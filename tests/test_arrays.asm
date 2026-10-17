; test_arrays.asm
; Checks array literals, Index/IndexSet, Add (append/concat), Len,
; independent nested repetitions and aliasing through dup/store, unlike
; the fresh copy created by each push.
;
; Expected output, in order:
; [1, 2, 3]
; 3
; 20
; [1, 2, 3, 4]
; [1, 2, 4]
; [0, 0, 0, 2, 2]
; [0, 0]
; [0, 0]
; [77, 0]
; 1
; true
; Error: index 99 out of bounds (length 3)
; Error: type mismatch in index

; --- basic literal and Index ---
push [1, 2, 3]
dup
call emit_value 1           ; [1, 2, 3]
pop
push 2
index
call emit_value 1           ; 3
pop

; --- another literal and an indexed read ---
push [10, 20, 30]
push 1
index
call emit_value 1           ; 20
pop

; --- Add: append and concat leave the inputs unchanged ---
push [1, 2, 3]
push 4
add
call emit_value 1           ; [1, 2, 3, 4]
pop

push [1, 2, 3]
push 0
push 4
indexset
call emit_value 1           ; [1, 2, 4]
pop

; --- literal repetition syntax ---
push [0 x 3, 2 x 2]
call emit_value 1           ; [0, 0, 0, 2, 2]
pop

; --- repeated nested literals must be INDEPENDENT ---
push [[0, 0] x 3]
store 0

load 0
push 0
index
call emit_value 1           ; [0, 0]        (first inner array, before mutation)
pop

load 0
push 1
index
push 0
push 77
indexset        ; set index 0 of the second inner array to 77

load 0
push 0
index
call emit_value 1           ; [0, 0]        (first inner array is unchanged)
pop

load 0
push 1
index
call emit_value 1           ; [77, 0]       (second inner array has changed)
pop

; --- a fresh literal on each push (NO aliasing) ---
push [1, 2]
store 0
push [1, 2]
store 1
load 0
push 0
push 99
indexset
load 1
push 0
index
call emit_value 1           ; 1             (mutating local0 leaves local1 unchanged)
pop

; --- dup/store aliasing (unlike push): a SHARED array ---
push [5, 5]
dup
store 0
store 1
load 0
push 0
push 42
indexset
load 1
push 0
index
push 42
eq
call emit_value 1           ; true          (local0 and local1 share the same reference)
pop

; --- errors ---
push [1, 2, 3]
push 99
index
call emit_value 1           ; Error: index 99 out of bounds (length 3)
pop

push [1, 2, 3]
push "x"
index
call emit_value 1           ; Error: type mismatch in index
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
