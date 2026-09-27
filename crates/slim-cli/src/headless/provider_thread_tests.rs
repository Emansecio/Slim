use super::{provider_runtime_block_on, AGENT_LOOP_STACK_BYTES};

#[test]
fn provider_futures_poll_on_a_dedicated_large_stack() {
    let (name, used) = provider_runtime_block_on(async {
        // Twice the Windows main-thread stack, in one poll.
        let frame = std::hint::black_box([1_u8; 2 * 1024 * 1024]);
        (
            std::thread::current().name().map(str::to_owned),
            frame.iter().map(|&byte| usize::from(byte)).sum::<usize>(),
        )
    })
    .expect("provider runtime");
    assert_eq!(name.as_deref(), Some("slim-provider"));
    assert_eq!(used, 2 * 1024 * 1024);
    const { assert!(AGENT_LOOP_STACK_BYTES >= 8 * 1024 * 1024) };
}

#[test]
fn provider_future_panic_resumes_on_the_caller() {
    let panic =
        std::panic::catch_unwind(|| provider_runtime_block_on(async { panic!("fixture panic") }))
            .expect_err("panic must reach the caller");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"fixture panic"));
}
