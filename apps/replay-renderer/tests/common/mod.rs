pub fn recording() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!([
        {"type":4,"timestamp":1000,"data":{"width":320,"height":240,"href":"https://example.test/"}},
        {"type":2,"timestamp":1000,"data":{"initialOffset":{"left":0,"top":0},"node":{"type":0,"id":1,"childNodes":[
            {"type":2,"id":2,"tagName":"html","attributes":{},"childNodes":[
                {"type":2,"id":3,"tagName":"head","attributes":{},"childNodes":[]},
                {"type":2,"id":4,"tagName":"body","attributes":{},"childNodes":[{"type":3,"id":5,"textContent":"Replay renderer test"}]}
            ]}
        ]}}},
        {"type":3,"timestamp":4200,"data":{"source":3,"id":4,"x":0,"y":0}}
    ])).unwrap()
}
