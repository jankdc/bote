import { open, fromFile } from '@botejs/core'
import { publish } from './message-bus'

await using cursor = await open(fromFile('./some-large.json'))

// .raw() to process each fetch's worth of orders concurrently with Promise.all
for await (const orders of cursor
  .iter('orders', {
    batch: 10,
    select: { id: 'id', status: 'status', total: ['payment', 'total'] },
    withKey: true,
  })
  .raw()) {
  const messages = await Promise.all(
    orders.map(async ([index, order]) => {
      if (order.status !== 'paid') {
        return null
      }

      const items = await cursor.hop('orders', index, 'items')
      const restock = []
      for await (const [sku] of items.iter({ withKey: true, select: 'sku' })) {
        const line = await items.hop(sku)
        if (!(await line.has('backorder'))) {
          continue
        }

        const short = await line.get('backorder', 'shortfall')
        const sources = []
        for await (const [warehouse, onHand] of line.iter('availability', { withKey: true })) {
          if (onHand > 0) {
            sources.push({ warehouse, onHand })
          }
        }

        restock.push({ sku, short, sources })
      }

      return restock.length ? { id: order.id, total: order.total, restock } : null
    }),
  )

  for (const message of messages) {
    if (message) {
      await publish('orders.fulfil', message)
    }
  }
}
