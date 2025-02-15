use completeio::event::Event;

#[test]
fn event_handle() {
    completeio::task::block_on(async {
        let event = Event::new().unwrap();
        let mut handle = event.handle().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| handle.notify().unwrap());
        });
        event.wait().await.unwrap();
    });
}
